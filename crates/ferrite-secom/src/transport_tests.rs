use super::*;
use crate::tests::{cert_at, fixture_at, key};
use openssl::{
    hash::MessageDigest,
    nid::Nid,
    pkcs12::Pkcs12,
    ssl::{SslAcceptor, SslMethod, SslVerifyMode},
    x509::{
        extension::{ExtendedKeyUsage, KeyUsage, SubjectAlternativeName},
        X509,
    },
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

// TLS libraries verify against the wall clock. Payload fixtures deliberately
// use a fixed historical instant; do not reuse their lifetime for live TLS.
fn cert(
    name: &str,
    key: &openssl::pkey::PKey<openssl::pkey::Private>,
    issuer: Option<(&X509, &openssl::pkey::PKey<openssl::pkey::Private>)>,
    ca: bool,
    digital: bool,
) -> X509 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    cert_at(name, key, issuer, ca, digital, now)
}

fn fixture(
    curve: Nid,
    algorithm: &str,
    digest: MessageDigest,
    intermediate: bool,
    digital: bool,
) -> (PayloadTrust, serde_json::Value) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    fixture_at(curve, algorithm, digest, intermediate, digital, now)
}

fn server_cert(
    key: &openssl::pkey::PKey<openssl::pkey::Private>,
    root: &X509,
    rk: &openssl::pkey::PKey<openssl::pkey::Private>,
    wrong_hostname: bool,
) -> X509 {
    // Rebuild the generated leaf to add the TLS hostname extension.
    let original = cert("localhost", key, Some((root, rk)), false, true);
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(original.serial_number()).unwrap();
    b.set_subject_name(original.subject_name()).unwrap();
    b.set_issuer_name(root.subject_name()).unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(original.not_before()).unwrap();
    b.set_not_after(original.not_after()).unwrap();
    b.append_extension(
        SubjectAlternativeName::new()
            .dns("localhost")
            .ip(if wrong_hostname {
                "127.0.0.2"
            } else {
                "127.0.0.1"
            })
            .build(&b.x509v3_context(Some(root), None))
            .unwrap(),
    )
    .unwrap();
    b.append_extension(ExtendedKeyUsage::new().server_auth().build().unwrap())
        .unwrap();
    b.append_extension(KeyUsage::new().digital_signature().build().unwrap())
        .unwrap();
    b.sign(rk, MessageDigest::sha384()).unwrap();
    b.build()
}
fn fixture_server(
    status: &str,
    body: String,
    trust: PayloadTrust,
    max: usize,
) -> (SecomClient, thread::JoinHandle<String>) {
    fixture_server_mode(status, body, trust, max, 0)
}
fn fixture_server_mode(
    status: &str,
    body: String,
    trust: PayloadTrust,
    max: usize,
    tls_mode: u8,
) -> (SecomClient, thread::JoinHandle<String>) {
    let rk = key(Nid::SECP384R1);
    let root = cert("TRANSPORT ROOT", &rk, None, true, true);
    let sk = key(Nid::SECP384R1);
    let server = server_cert(&sk, &root, &rk, tls_mode == 2);
    let ck = key(Nid::SECP384R1);
    let client = cert("TEST CLIENT", &ck, Some((&root, &rk)), false, true);
    let mut p12 = Pkcs12::builder();
    p12.name("test").pkey(&ck).cert(&client);
    let identity = p12.build2("local-test").unwrap().to_der().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&sk).unwrap();
    acceptor.set_certificate(&server).unwrap();
    acceptor.cert_store_mut().add_cert(root.clone()).unwrap();
    acceptor.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    let acceptor = acceptor.build();
    let status = status.to_owned();
    let handle = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        socket
            .set_write_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut stream = match acceptor.accept(socket) {
            Ok(stream) => stream,
            Err(_) => return "TLS_REJECTED".to_string(),
        };
        assert_eq!(
            stream
                .ssl()
                .peer_certificate()
                .unwrap()
                .subject_name()
                .entries()
                .next()
                .unwrap()
                .data()
                .to_string()
                .unwrap(),
            "TEST CLIENT"
        );
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = match stream.read(&mut buf) {
                Ok(0) | Err(_) if request.is_empty() => return "TLS_REJECTED".into(),
                Ok(n) => n,
                Err(error) => panic!("HTTP read failed: {error}"),
            };
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
            assert!(request.len() < 8192);
        }
        write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        stream.flush().unwrap();
        String::from_utf8(request).unwrap()
    });
    let configured_root = if tls_mode == 1 {
        let other_key = key(Nid::SECP384R1);
        cert("OTHER ROOT", &other_key, None, true, true)
    } else {
        root
    };
    let client = SecomClient::new(
        &format!("https://127.0.0.1:{port}/"),
        &configured_root.to_pem().unwrap(),
        &identity,
        "local-test",
        trust,
        max,
    )
    .unwrap();
    (client, handle)
}
#[test]
fn actual_mtls_get_returns_only_authenticated_payload_and_encoded_uuid_query() {
    let (trust, message) = fixture(
        Nid::SECP384R1,
        "ecdsa-384-sha2",
        MessageDigest::sha384(),
        false,
        true,
    );
    let body=serde_json::json!({"dataResponseObject":[message],"pagination":{"totalItems":1,"maxItemsPerPage":1}}).to_string();
    let (client, server) = fixture_server("200 OK", body, trust, 65536);
    let id = uuid::Uuid::parse_str("3a1f4800-3333-4444-8888-000000000001").unwrap();
    let result = client.get(id, 1, 1);
    let request = server.join().unwrap();
    let page = result.unwrap();
    assert_eq!(page.payloads[0].bytes(), b"signed chart bytes\x00\xff");
    assert!(request.starts_with("GET /v1/object?dataReference=3a1f4800-3333-4444-8888-000000000001&page=1&pageSize=1 HTTP/1.1"));
}
#[test]
fn actual_mtls_rejects_redirect_oversized_response_and_tampered_payload() {
    for mode in [0, 1, 2] {
        let (trust, mut message) = fixture(
            Nid::SECP384R1,
            "ecdsa-384-sha2",
            MessageDigest::sha384(),
            false,
            true,
        );
        if mode == 2 {
            message["data"] = STANDARD.encode(b"tampered").into();
        }
        let body = serde_json::json!({"dataResponseObject":[message],"pagination":{}}).to_string();
        let (client, server) = fixture_server(
            if mode == 0 { "302 Found" } else { "200 OK" },
            body,
            trust,
            if mode == 1 { 16 } else { 65536 },
        );
        assert!(client.get(uuid::Uuid::nil(), 1, 1).is_err());
        assert!(server.join().unwrap().starts_with("GET /v1/object?"));
    }
}

#[test]
fn actual_tls_rejects_untrusted_server_and_wrong_hostname_before_http() {
    for mode in [1, 2] {
        let (trust, _) = fixture(
            Nid::SECP384R1,
            "ecdsa-384-sha2",
            MessageDigest::sha384(),
            false,
            true,
        );
        let (client, server) = fixture_server_mode("200 OK", "{}".into(), trust, 65536, mode);
        assert!(client.ping().is_err());
        assert_eq!(server.join().unwrap(), "TLS_REJECTED");
    }
}
