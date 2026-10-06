use super::*;
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    sign::Signer,
    x509::{
        extension::{BasicConstraints, KeyUsage},
        X509NameBuilder, X509,
    },
};
const NOW: i64 = 1_791_043_200;
pub(super) fn key(curve: Nid) -> PKey<Private> {
    PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(curve).unwrap()).unwrap()).unwrap()
}
pub(super) fn cert(
    name: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    ca: bool,
    digital: bool,
) -> X509 {
    cert_at(name, key, issuer, ca, digital, NOW)
}
pub(super) fn cert_at(
    name: &str,
    key: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    ca: bool,
    digital: bool,
    now: i64,
) -> X509 {
    let mut n = X509NameBuilder::new().unwrap();
    n.append_entry_by_text("CN", name).unwrap();
    let n = n.build();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(
        &BigNum::from_u32(if ca { 1 } else { 2 })
            .unwrap()
            .to_asn1_integer()
            .unwrap(),
    )
    .unwrap();
    b.set_subject_name(&n).unwrap();
    b.set_issuer_name(issuer.map(|i| i.0.subject_name()).unwrap_or(&n))
        .unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(&Asn1Time::from_unix(now - 3600).unwrap())
        .unwrap();
    b.set_not_after(&Asn1Time::from_unix(now + 86400).unwrap())
        .unwrap();
    let mut bc = BasicConstraints::new();
    bc.critical();
    if ca {
        bc.ca();
    }
    b.append_extension(bc.build().unwrap()).unwrap();
    let mut ku = KeyUsage::new();
    ku.critical();
    if ca {
        ku.key_cert_sign().crl_sign();
    } else if digital {
        ku.digital_signature();
    } else {
        ku.key_encipherment();
    }
    b.append_extension(ku.build().unwrap()).unwrap();
    b.sign(issuer.map(|i| i.1).unwrap_or(key), MessageDigest::sha384())
        .unwrap();
    b.build()
}
pub(super) fn fixture(
    curve: Nid,
    algorithm: &str,
    digest: MessageDigest,
    intermediate: bool,
    digital: bool,
) -> (PayloadTrust, serde_json::Value) {
    fixture_at(curve, algorithm, digest, intermediate, digital, NOW)
}
pub(super) fn fixture_at(
    curve: Nid,
    algorithm: &str,
    digest: MessageDigest,
    intermediate: bool,
    digital: bool,
    now: i64,
) -> (PayloadTrust, serde_json::Value) {
    let rk = key(Nid::SECP384R1);
    let root = cert_at("SECOM ROOT", &rk, None, true, true, now);
    let ik = key(Nid::SECP384R1);
    let inter = cert_at("INTER", &ik, Some((&root, &rk)), true, true, now);
    let lk = key(curve);
    let issuer = if intermediate {
        (&inter, &ik)
    } else {
        (&root, &rk)
    };
    let leaf = cert_at("PRODUCER", &lk, Some(issuer), false, digital, now);
    let bytes = b"signed chart bytes\x00\xff";
    let mut signer = Signer::new(digest, &lk).unwrap();
    signer.update(bytes).unwrap();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
    let message = serde_json::json!({"data":STANDARD.encode(bytes), "ackRequest":3, "exchangeMetadata":{
        "dataProtection":false,"protectionScheme":"", "compressionFlag":false,"digitalSignatureReference":algorithm,
        "digitalSignatureValue":{"publicRootCertificateThumbprint":hex(&root.digest(MessageDigest::sha256()).unwrap()),
            "publicCertificate":STANDARD.encode(leaf.to_der().unwrap()),"digitalSignature":hex(&signer.sign_to_vec().unwrap())}}});
    (
        PayloadTrust::new(
            root.to_pem().unwrap(),
            if intermediate {
                vec![inter.to_der().unwrap()]
            } else {
                vec![]
            },
            1024,
        )
        .unwrap(),
        message,
    )
}
fn authenticate(
    trust: &PayloadTrust,
    json: &serde_json::Value,
    time: i64,
) -> Result<VerifiedPayload> {
    trust.authenticate(serde_json::from_value(json.clone())?, time)
}
#[test]
fn authenticates_all_four_ecdsa_wire_profiles_with_intermediate() {
    for (curve, algorithm, digest) in [
        (
            Nid::X9_62_PRIME256V1,
            "ecdsa-256-sha2-256",
            MessageDigest::sha256(),
        ),
        (
            Nid::X9_62_PRIME256V1,
            "ecdsa-256-sha3-256",
            MessageDigest::sha3_256(),
        ),
        (Nid::SECP384R1, "ecdsa-384-sha2", MessageDigest::sha384()),
        (Nid::SECP384R1, "ecdsa-384-sha3", MessageDigest::sha3_384()),
    ] {
        let (trust, json) = fixture(curve, algorithm, digest, true, true);
        let payload = authenticate(&trust, &json, NOW).unwrap();
        assert_eq!(payload.bytes(), b"signed chart bytes\x00\xff");
        assert_eq!(payload.ack_request(), 3);
    }
}
#[test]
fn rejects_tampering_untrusted_roots_bad_usage_curve_and_validity() {
    let (trust, json) = fixture(
        Nid::SECP384R1,
        "ecdsa-384-sha2",
        MessageDigest::sha384(),
        false,
        true,
    );
    let mut bad = json.clone();
    bad["data"] = STANDARD.encode(b"tampered").into();
    assert!(authenticate(&trust, &bad, NOW).is_err());
    let (other, _) = fixture(
        Nid::SECP384R1,
        "ecdsa-384-sha2",
        MessageDigest::sha384(),
        false,
        true,
    );
    assert!(authenticate(&other, &json, NOW).is_err());
    // Removing the thumbprint cannot turn an untrusted certificate into a trusted one.
    bad = json.clone();
    bad["exchangeMetadata"]["digitalSignatureValue"]["publicRootCertificateThumbprint"] =
        serde_json::Value::Null;
    assert!(authenticate(&other, &bad, NOW).is_err());
    assert!(authenticate(&trust, &bad, NOW).is_ok());
    assert!(authenticate(&trust, &json, NOW + 90000).is_err());
    assert!(authenticate(&trust, &json, NOW - 7200).is_err());
    let (trust, bad) = fixture(
        Nid::SECP384R1,
        "ecdsa-384-sha2",
        MessageDigest::sha384(),
        false,
        false,
    );
    assert!(authenticate(&trust, &bad, NOW).is_err());
    let (trust, bad) = fixture(
        Nid::X9_62_PRIME256V1,
        "ecdsa-384-sha2",
        MessageDigest::sha384(),
        false,
        true,
    );
    assert!(authenticate(&trust, &bad, NOW).is_err());
}
#[test]
fn rejects_unsupported_transform_algorithm_and_malformed_wire_values() {
    let (trust, json) = fixture(
        Nid::SECP384R1,
        "ecdsa-384-sha2",
        MessageDigest::sha384(),
        false,
        true,
    );
    for flag in ["compressionFlag", "dataProtection"] {
        let mut bad = json.clone();
        bad["exchangeMetadata"][flag] = true.into();
        assert!(authenticate(&trust, &bad, NOW).is_err());
    }
    for alg in ["dsa", "cvc_ecdsa", "invented"] {
        let mut bad = json.clone();
        bad["exchangeMetadata"]["digitalSignatureReference"] = alg.into();
        assert!(authenticate(&trust, &bad, NOW).is_err());
    }
    for sig in ["", "ABC", "GG", "00"] {
        let mut bad = json.clone();
        bad["exchangeMetadata"]["digitalSignatureValue"]["digitalSignature"] = sig.into();
        assert!(authenticate(&trust, &bad, NOW).is_err());
    }
    let mut bad = json.clone();
    bad["data"] = "not base64!".into();
    assert!(authenticate(&trust, &bad, NOW).is_err());
    bad = json.clone();
    bad["ackRequest"] = 4.into();
    assert!(authenticate(&trust, &bad, NOW).is_err());
    bad = json.clone();
    bad["data"] = STANDARD.encode(vec![0; 1025]).into();
    assert!(authenticate(&trust, &bad, NOW).is_err());
    bad = json.clone();
    bad["exchangeMetadata"]["digitalSignatureValue"]["publicRootCertificateThumbprint"] =
        "A".repeat(64).into();
    assert!(authenticate(&trust, &bad, NOW).is_err());
}
