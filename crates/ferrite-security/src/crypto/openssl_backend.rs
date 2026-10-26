//! The original OpenSSL implementation, unchanged in behaviour.
use super::{Cert, Curve, Inner, MessageHash};
use anyhow::Result;
use openssl::{
    ecdsa::EcdsaSig,
    hash::MessageDigest,
    nid::Nid,
    sign::Verifier,
    stack::Stack,
    x509::{
        store::X509StoreBuilder,
        verify::{X509VerifyFlags, X509VerifyParam},
        X509StoreContext, X509,
    },
};

fn wrap(cert: X509) -> Result<Cert> {
    Ok(Cert {
        der: cert.to_der()?.into(),
        subject: cert.subject_name().to_der()?.into(),
        issuer: cert.issuer_name().to_der()?.into(),
        inner: Inner::OpenSsl(cert),
    })
}

pub(super) fn from_der(der: &[u8]) -> Result<Cert> {
    wrap(X509::from_der(der)?)
}

pub(super) fn stack_from_pem(pem: &[u8]) -> Result<Vec<Cert>> {
    X509::stack_from_pem(pem)?.into_iter().map(wrap).collect()
}

pub(super) fn verifies_own_signature(cert: &X509) -> Result<bool> {
    let key = cert.public_key()?;
    Ok(cert.verify(&key)?)
}

pub(super) fn subject_common_names(cert: &X509) -> Vec<Vec<u8>> {
    cert.subject_name()
        .entries_by_nid(Nid::COMMONNAME)
        .map(|e| e.data().as_slice().to_vec())
        .collect()
}

pub(super) fn ec_curve(cert: &X509) -> Result<Curve> {
    let key = cert.public_key()?;
    Ok(match key.ec_key()?.group().curve_name() {
        Some(Nid::SECP384R1) => Curve::P384,
        Some(Nid::X9_62_PRIME256V1) => Curve::P256,
        _ => Curve::Other,
    })
}

pub(super) fn verify_ecdsa_digest(cert: &X509, digest: &[u8], signature: &[u8]) -> Result<bool> {
    let key = cert.public_key()?.ec_key()?;
    Ok(EcdsaSig::from_der(signature)?.verify(digest, &key)?)
}

pub(super) fn verify_ecdsa_message(
    cert: &X509,
    hash: MessageHash,
    data: &[u8],
    signature: &[u8],
) -> Result<bool> {
    let digest = match hash {
        MessageHash::Sha256 => MessageDigest::sha256(),
        MessageHash::Sha3_256 => MessageDigest::sha3_256(),
        MessageHash::Sha384 => MessageDigest::sha384(),
        MessageHash::Sha3_384 => MessageDigest::sha3_384(),
    };
    let key = cert.public_key()?;
    let mut verifier = Verifier::new(digest, &key)?;
    verifier.update(data)?;
    Ok(verifier.verify(signature)?)
}

pub(super) fn canonical_ecdsa_der(bytes: &[u8]) -> Result<bool> {
    Ok(EcdsaSig::from_der(bytes)?.to_der()? == bytes)
}

pub(super) fn verify_path(
    leaf: &X509,
    chain: &[X509],
    root: &X509,
    time: i64,
) -> std::result::Result<(), String> {
    let run = || -> Result<(bool, String)> {
        let mut untrusted = Stack::new()?;
        for cert in chain {
            untrusted.push(cert.clone())?;
        }
        let mut store = X509StoreBuilder::new()?;
        store.add_cert(root.clone())?;
        let mut param = X509VerifyParam::new()?;
        param.set_time(time);
        param.set_flags(X509VerifyFlags::CHECK_SS_SIGNATURE)?;
        store.set_param(&param)?;
        let mut context = X509StoreContext::new()?;
        Ok(context.init(&store.build(), leaf, &untrusted, |ctx| {
            let valid = ctx.verify_cert()?;
            Ok((valid, ctx.error().to_string()))
        })?)
    };
    match run() {
        Ok((true, _)) => Ok(()),
        Ok((false, error)) => Err(error),
        Err(error) => Err(format!("{error:#}")),
    }
}
