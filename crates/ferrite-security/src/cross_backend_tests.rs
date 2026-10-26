//! Verdict parity of the OpenSSL and pure-Rust crypto backends on the same
//! inputs (ENGINE_DESIGN 4a: security behaviour must not depend on the
//! backend). Where the Rust backend deliberately fails closed, the test
//! asserts that it rejects and records what OpenSSL decides.
use super::*;
use openssl::{
    asn1::{Asn1Object, Asn1OctetString},
    bn::{BigNum, BigNumContext},
    ec::{Asn1Flag, EcGroup, EcKey},
    ecdsa::EcdsaSig,
    x509::{
        extension::{AuthorityKeyIdentifier, SubjectKeyIdentifier},
        X509Extension,
    },
};

const BACKENDS: [CryptoBackend; 2] = [CryptoBackend::OpenSsl, CryptoBackend::Rust];

fn anchors_for(pem: &[u8], backend: CryptoBackend) -> TrustAnchors {
    let mut anchors = TrustAnchors::with_backend(backend);
    anchors.install_pem("IHO", pem).unwrap();
    anchors
}

/// Everything a verification report asserts, for byte-exact comparison.
fn summary(report: &VerificationReport) -> String {
    let mut out = format!(
        "catalogue {} {} {:?}\n",
        report.catalogue.sha384, report.catalogue.size, report.catalogue.signature_ids
    );
    for r in &report.resources {
        out += &format!(
            "resource {} {} {} {:?} {:?}\n",
            r.path.display(),
            r.sha384,
            r.size,
            r.signature_ids,
            r.certificate_ids
        );
        for d in &r.authentication.signatures {
            out += &format!(
                "  sig {} {} {} {}\n",
                d.id,
                d.signer_certificate_sha256,
                hex(&d.der),
                hex(&d.signer_certificate_der)
            );
        }
    }
    let mut warnings = report.metadata_warnings.clone();
    warnings.sort();
    let mut anchors: Vec<_> = report.trust_anchor_sha256.iter().collect();
    anchors.sort();
    out += &format!(
        "discovery {} warnings {warnings:?} anchors {anchors:?}",
        report.dataset_discovery.len()
    );
    out
}

/// Verify `root` with both backends; the verdicts and reports must match.
fn exchange_parity(case: &str, root: &Path, pem: &[u8], time: i64) -> Result<String, String> {
    let results: Vec<_> = BACKENDS
        .iter()
        .map(|b| verify_exchange(root, &anchors_for(pem, *b), time))
        .collect();
    match (&results[0], &results[1]) {
        (Ok(a), Ok(b)) => {
            assert_eq!(summary(a), summary(b), "{case}: reports differ");
            Ok(summary(a))
        }
        (Err(a), Err(b)) => {
            // Same failure class for the reasons tests rely on.
            for reason in ["expired", "not yet valid"] {
                assert_eq!(
                    format!("{a:#}").contains(reason),
                    format!("{b:#}").contains(reason),
                    "{case}: OpenSSL `{a:#}` vs Rust `{b:#}`"
                );
            }
            Err(format!("{b:#}"))
        }
        (a, b) => panic!(
            "{case}: verdicts differ: OpenSSL {:?} / Rust {:?}",
            a.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")),
            b.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"))
        ),
    }
}

fn fixture_parity(case: &str, f: &Fixture, time: i64) -> Result<String, String> {
    exchange_parity(case, f.dir.path(), &f.root_pem, time)
}

#[test]
fn synthetic_exchange_sets_give_identical_verdicts() {
    let mut accepted = 0;
    for intermediate in [false, true] {
        for digital in [false, true] {
            for curve in [Nid::SECP384R1, Nid::X9_62_PRIME256V1] {
                for chained in [false, true] {
                    let f = Fixture::new(intermediate, digital, curve);
                    f.write("file:/DATA.H5", chained, "LEAF", "unencrypted");
                    let case = format!("i={intermediate} d={digital} {curve:?} c={chained}");
                    // Fixture validity: [NOW - 3600, NOW + 86400).
                    for time in [
                        NOW - 3601,
                        NOW - 3600,
                        NOW,
                        NOW + 86399,
                        NOW + 86400,
                        NOW + 86401,
                    ] {
                        let verdict = fixture_parity(&format!("{case} t={time}"), &f, time);
                        let valid_time = (NOW - 3600..NOW + 86400).contains(&time);
                        let expected = valid_time && digital && curve == Nid::SECP384R1;
                        assert_eq!(verdict.is_ok(), expected, "{case} t={time}: {verdict:?}");
                        accepted += usize::from(verdict.is_ok());
                    }
                }
            }
        }
    }
    assert_eq!(accepted, 2 * 2 * 3);
}

#[test]
fn tampering_and_references_give_identical_verdicts() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    assert!(fixture_parity("clean", &f, NOW).is_ok());
    // Tampered resource.
    std::fs::write(
        f.dir.path().join("DATA.H5"),
        b"authentic bathymetry bytes\x00\xfe",
    )
    .unwrap();
    assert!(fixture_parity("tampered resource", &f, NOW).is_err());
    // Tampered catalogue (signature no longer matches).
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let mut catalogue = std::fs::read(f.dir.path().join("CATALOG.XML")).unwrap();
    let at = catalogue.len() - 10;
    catalogue[at] ^= 1;
    std::fs::write(f.dir.path().join("CATALOG.XML"), catalogue).unwrap();
    assert!(fixture_parity("tampered catalogue", &f, NOW).is_err());
    // Signing with the intermediate CA certificate, unknown references.
    for reference in ["INTER", "MISSING"] {
        f.write("file:/DATA.H5", false, reference, "unencrypted");
        assert!(fixture_parity(reference, &f, NOW).is_err());
    }
    // A signature over other bytes.
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    std::fs::write(f.dir.path().join("DATA.H5"), b"other").unwrap();
    assert!(fixture_parity("other bytes", &f, NOW).is_err());
}

#[test]
fn rogue_certificates_give_identical_verdicts() {
    // Unreferenced rogue: ignored by both.
    let mut f = Fixture::new(false, true, Nid::SECP384R1);
    f.container += &rogue_certificate("ROGUE");
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let report = fixture_parity("unreferenced rogue", &f, NOW).unwrap();
    assert!(report.contains("Certificate ROGUE"));
    // Referenced rogue: rejected by both.
    f.write("file:/DATA.H5", false, "ROGUE", "unencrypted");
    assert!(fixture_parity("referenced rogue", &f, NOW).is_err());
    // A rogue root with the installed root's name issues the signer.
    let rogue_root_key = key(Nid::SECP384R1);
    let rogue_root = certificate("ROOT", &rogue_root_key, None, true, true);
    let signer_key = key(Nid::SECP384R1);
    let signer = certificate(
        "PRODUCER",
        &signer_key,
        Some((&rogue_root, &rogue_root_key)),
        false,
        true,
    );
    let mut f = Fixture::new(false, true, Nid::SECP384R1);
    f.container = format!(
        "<se:schemeAdministrator id=\"IHO\"/><se:certificate id=\"LEAF\" issuer=\"IHO\">{}</se:certificate>",
        STANDARD.encode(signer.to_der().unwrap())
    );
    f.key = signer_key;
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let error = fixture_parity("rogue root of the same name", &f, NOW).unwrap_err();
    assert!(error.contains("validation failed"), "{error}");
    // A rogue intermediate: the issuer label resolves, the key does not.
    let mut f = Fixture::new(true, true, Nid::SECP384R1);
    let rogue_inter_key = key(Nid::SECP384R1);
    let rogue_inter = certificate(
        "INTER",
        &rogue_inter_key,
        Some((&rogue_root, &rogue_root_key)),
        true,
        true,
    );
    let start = f.container.find("<se:certificate id=\"INTER\"").unwrap();
    f.container.truncate(start);
    f.container += &format!(
        "<se:certificate id=\"INTER\" issuer=\"IHO\">{}</se:certificate>",
        STANDARD.encode(rogue_inter.to_der().unwrap())
    );
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    assert!(fixture_parity("rogue intermediate", &f, NOW).is_err());
}

// ── Certificate path validation, case by case ─────────────────────────────

struct Spec<'a> {
    name: &'a str,
    key: &'a PKey<Private>,
    issuer: Option<(&'a X509, &'a PKey<Private>)>,
    validity: (i64, i64),
    /// basicConstraints (critical, CA, pathLen).
    basic: Option<(bool, bool, Option<u32>)>,
    /// keyUsage names ("cert", "crl", "sig", "enc").
    usage: Option<&'a [&'a str]>,
    key_ids: bool,
    extensions: Vec<X509Extension>,
    digest: MessageDigest,
    v1: bool,
    serial: u32,
}

impl<'a> Spec<'a> {
    fn ca(
        name: &'a str,
        key: &'a PKey<Private>,
        issuer: Option<(&'a X509, &'a PKey<Private>)>,
    ) -> Self {
        Self {
            name,
            key,
            issuer,
            validity: (NOW - 3600, NOW + 86400),
            basic: Some((true, true, None)),
            usage: Some(&["cert", "crl"]),
            key_ids: true,
            extensions: Vec::new(),
            digest: MessageDigest::sha384(),
            v1: false,
            serial: 1,
        }
    }
    fn leaf(name: &'a str, key: &'a PKey<Private>, issuer: (&'a X509, &'a PKey<Private>)) -> Self {
        Self {
            basic: Some((true, false, None)),
            usage: Some(&["sig"]),
            serial: 2,
            ..Self::ca(name, key, Some(issuer))
        }
    }
    fn build(self) -> X509 {
        use openssl::x509::extension::{BasicConstraints, KeyUsage};
        let mut n = X509NameBuilder::new().unwrap();
        n.append_entry_by_text("CN", self.name).unwrap();
        let n = n.build();
        let mut b = X509::builder().unwrap();
        b.set_version(if self.v1 { 0 } else { 2 }).unwrap();
        b.set_serial_number(
            &BigNum::from_u32(self.serial)
                .unwrap()
                .to_asn1_integer()
                .unwrap(),
        )
        .unwrap();
        b.set_subject_name(&n).unwrap();
        b.set_issuer_name(self.issuer.map(|i| i.0.subject_name()).unwrap_or(&n))
            .unwrap();
        b.set_pubkey(self.key).unwrap();
        b.set_not_before(&Asn1Time::from_unix(self.validity.0).unwrap())
            .unwrap();
        b.set_not_after(&Asn1Time::from_unix(self.validity.1).unwrap())
            .unwrap();
        if !self.v1 {
            if let Some((critical, ca, pathlen)) = self.basic {
                let mut c = BasicConstraints::new();
                if critical {
                    c.critical();
                }
                if ca {
                    c.ca();
                }
                if let Some(p) = pathlen {
                    c.pathlen(p);
                }
                b.append_extension(c.build().unwrap()).unwrap();
            }
            if let Some(names) = self.usage {
                let mut u = KeyUsage::new();
                u.critical();
                for name in names {
                    match *name {
                        "cert" => u.key_cert_sign(),
                        "crl" => u.crl_sign(),
                        "sig" => u.digital_signature(),
                        "enc" => u.key_encipherment(),
                        other => panic!("{other}"),
                    };
                }
                b.append_extension(u.build().unwrap()).unwrap();
            }
            if self.key_ids {
                let skid = SubjectKeyIdentifier::new()
                    .build(&b.x509v3_context(None, None))
                    .unwrap();
                b.append_extension(skid).unwrap();
                if let Some((issuer, _)) = self.issuer.filter(|i| i.0.subject_key_id().is_some()) {
                    let akid = AuthorityKeyIdentifier::new()
                        .keyid(true)
                        .build(&b.x509v3_context(Some(issuer), None))
                        .unwrap();
                    b.append_extension(akid).unwrap();
                }
            }
            for ext in self.extensions {
                b.append_extension(ext).unwrap();
            }
        }
        b.sign(self.issuer.map(|i| i.1).unwrap_or(self.key), self.digest)
            .unwrap();
        b.build()
    }
}

fn raw_extension(oid: &str, critical: bool, der: &[u8]) -> X509Extension {
    X509Extension::new_from_der(
        &Asn1Object::from_str(oid).unwrap(),
        critical,
        &Asn1OctetString::new_from_bytes(der).unwrap(),
    )
    .unwrap()
}

/// [OpenSSL verdict, Rust verdict] of one path.
fn path_verdicts(leaf: &X509, chain: &[&X509], root: &X509, time: i64) -> [bool; 2] {
    BACKENDS.map(|backend| {
        let parse = |c: &X509| Cert::from_der(backend, &c.to_der().unwrap());
        let (Ok(leaf), Ok(root)) = (parse(leaf), parse(root)) else {
            return false;
        };
        let Ok(chain) = chain.iter().map(|c| parse(c)).collect::<Result<Vec<_>>>() else {
            return false;
        };
        crypto::verify_path(&leaf, &chain, &root, time).is_ok()
    })
}

fn same(case: &str, verdicts: [bool; 2], expected: bool) {
    assert_eq!(
        verdicts,
        [expected, expected],
        "{case}: [OpenSSL, Rust] verdicts"
    );
}

struct Keys {
    root: PKey<Private>,
    inter: PKey<Private>,
    inter2: PKey<Private>,
    leaf: PKey<Private>,
}
fn keys() -> Keys {
    Keys {
        root: key(Nid::SECP384R1),
        inter: key(Nid::SECP384R1),
        inter2: key(Nid::SECP384R1),
        leaf: key(Nid::SECP384R1),
    }
}

#[test]
fn path_validation_times_and_signatures_match_openssl() {
    let k = keys();
    let root = Spec::ca("ROOT", &k.root, None).build();
    let inter = Spec::ca("INTER", &k.inter, Some((&root, &k.root))).build();
    let leaf = Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter)).build();
    for (time, expected) in [
        (NOW - 3601, false),
        (NOW - 3600, true),
        (NOW, true),
        (NOW + 86399, true),
        (NOW + 86400, false),
    ] {
        same(
            &format!("t={time}"),
            path_verdicts(&leaf, &[&inter], &root, time),
            expected,
        );
    }
    // Missing intermediate, wrong root, wrong order.
    same(
        "missing intermediate",
        path_verdicts(&leaf, &[], &root, NOW),
        false,
    );
    let other_key = key(Nid::SECP384R1);
    let other_root = Spec::ca("OTHER ROOT", &other_key, None).build();
    same(
        "other root",
        path_verdicts(&leaf, &[&inter], &other_root, NOW),
        false,
    );
    // Expired root, not-yet-valid intermediate.
    let old_root = Spec {
        validity: (NOW - 7200, NOW - 1),
        ..Spec::ca("ROOT", &k.root, None)
    }
    .build();
    let inter_old = Spec::ca("INTER", &k.inter, Some((&old_root, &k.root))).build();
    let leaf_old = Spec::leaf("LEAF", &k.leaf, (&inter_old, &k.inter)).build();
    same(
        "expired root",
        path_verdicts(&leaf_old, &[&inter_old], &old_root, NOW),
        false,
    );
    let future = Spec {
        validity: (NOW + 10, NOW + 86400),
        ..Spec::ca("INTER", &k.inter, Some((&root, &k.root)))
    }
    .build();
    let leaf_future = Spec::leaf("LEAF", &k.leaf, (&future, &k.inter)).build();
    same(
        "future intermediate",
        path_verdicts(&leaf_future, &[&future], &root, NOW),
        false,
    );
    // Signed by a key other than the issuer's.
    let forged = Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter2)).build();
    same(
        "forged signature",
        path_verdicts(&forged, &[&inter], &root, NOW),
        false,
    );
    // Rogue root of the same name: names match, keys do not.
    let rogue_key = key(Nid::SECP384R1);
    let rogue_root = Spec::ca("ROOT", &rogue_key, None).build();
    let rogue_inter = Spec {
        key_ids: false,
        ..Spec::ca("INTER", &k.inter, Some((&rogue_root, &rogue_key)))
    }
    .build();
    let rogue_leaf = Spec::leaf("LEAF", &k.leaf, (&rogue_inter, &k.inter)).build();
    same(
        "rogue root",
        path_verdicts(&rogue_leaf, &[&rogue_inter], &root, NOW),
        false,
    );
    // P-256 anchor with SHA-256 signatures throughout.
    let p256 = key(Nid::X9_62_PRIME256V1);
    let root256 = Spec {
        digest: MessageDigest::sha256(),
        ..Spec::ca("ROOT256", &p256, None)
    }
    .build();
    let leaf256 = Spec {
        digest: MessageDigest::sha256(),
        ..Spec::leaf("LEAF256", &k.leaf, (&root256, &p256))
    }
    .build();
    same(
        "P-256 root",
        path_verdicts(&leaf256, &[], &root256, NOW),
        true,
    );
    // SHA-512 and SHA-224 certificate signatures.
    for digest in [MessageDigest::sha512(), MessageDigest::sha224()] {
        let leaf = Spec {
            digest,
            ..Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter))
        }
        .build();
        same(
            "other SHA-2 digest",
            path_verdicts(&leaf, &[&inter], &root, NOW),
            true,
        );
    }
}

#[test]
fn path_validation_extensions_match_openssl() {
    let k = keys();
    let root = Spec::ca("ROOT", &k.root, None).build();
    let inter = Spec::ca("INTER", &k.inter, Some((&root, &k.root))).build();
    let leaf_of = |spec: Spec| spec.build();
    let ok = leaf_of(Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter)));
    same("baseline", path_verdicts(&ok, &[&inter], &root, NOW), true);

    // CA status of intermediates and roots.
    let check_inter = |case: &str, spec: Spec, expected: bool| {
        let inter = spec.build();
        let leaf = Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter)).build();
        same(case, path_verdicts(&leaf, &[&inter], &root, NOW), expected);
    };
    let base = || Spec::ca("INTER", &k.inter, Some((&root, &k.root)));
    check_inter(
        "intermediate without basicConstraints",
        Spec {
            basic: None,
            ..base()
        },
        false,
    );
    check_inter(
        "intermediate CA:FALSE",
        Spec {
            basic: Some((true, false, None)),
            ..base()
        },
        false,
    );
    check_inter(
        "intermediate keyUsage without keyCertSign",
        Spec {
            usage: Some(&["sig"]),
            ..base()
        },
        false,
    );
    check_inter(
        "intermediate without keyUsage",
        Spec {
            usage: None,
            ..base()
        },
        true,
    );
    check_inter(
        "intermediate non-critical basicConstraints",
        Spec {
            basic: Some((false, true, None)),
            ..base()
        },
        true,
    );
    check_inter(
        "intermediate pathLen 0 above the leaf",
        Spec {
            basic: Some((true, true, Some(0))),
            ..base()
        },
        true,
    );
    let check_root = |case: &str, spec: Spec, expected: bool| {
        let root = spec.build();
        let leaf = Spec::leaf("LEAF", &k.leaf, (&root, &k.root)).build();
        same(case, path_verdicts(&leaf, &[], &root, NOW), expected);
    };
    let base_root = || Spec::ca("ROOT", &k.root, None);
    check_root(
        "root without basicConstraints, keyUsage keyCertSign",
        Spec {
            basic: None,
            ..base_root()
        },
        true,
    );
    check_root(
        "root without basicConstraints and keyUsage",
        Spec {
            basic: None,
            usage: None,
            ..base_root()
        },
        false,
    );
    check_root(
        "root keyUsage without keyCertSign",
        Spec {
            usage: Some(&["sig"]),
            ..base_root()
        },
        false,
    );
    check_root(
        "root CA:FALSE",
        Spec {
            basic: Some((true, false, None)),
            ..base_root()
        },
        false,
    );
    check_root(
        "V1 root",
        Spec {
            v1: true,
            ..base_root()
        },
        true,
    );
    // Path length constraints on the root above one or two intermediates.
    for (pathlen, expected) in [(0, false), (1, true)] {
        let root = Spec {
            basic: Some((true, true, Some(pathlen))),
            ..Spec::ca("ROOT", &k.root, None)
        }
        .build();
        let inter = Spec::ca("INTER", &k.inter, Some((&root, &k.root))).build();
        let leaf = Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter)).build();
        same(
            &format!("root pathLen {pathlen}"),
            path_verdicts(&leaf, &[&inter], &root, NOW),
            expected,
        );
    }
    for (pathlen, expected) in [(0, false), (1, true)] {
        let top = Spec {
            basic: Some((true, true, Some(pathlen))),
            ..Spec::ca("TOP", &k.inter2, Some((&root, &k.root)))
        }
        .build();
        let inter = Spec::ca("INTER", &k.inter, Some((&top, &k.inter2))).build();
        let leaf = Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter)).build();
        same(
            &format!("intermediate pathLen {pathlen} above another"),
            path_verdicts(&leaf, &[&inter, &top], &root, NOW),
            expected,
        );
    }
    // Leaf extensions.
    let leaf_ext = |case: &str, ext: X509Extension, expected: bool| {
        let leaf = Spec {
            extensions: vec![ext],
            ..Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter))
        }
        .build();
        same(case, path_verdicts(&leaf, &[&inter], &root, NOW), expected);
    };
    leaf_ext(
        "unknown critical extension",
        raw_extension("1.2.3.4", true, &[5, 0]),
        false,
    );
    leaf_ext(
        "unknown non-critical extension",
        raw_extension("1.2.3.4", false, &[5, 0]),
        true,
    );
    leaf_ext(
        "critical extended key usage (handled)",
        raw_extension(
            "2.5.29.37",
            true,
            &[0x30, 0x0a, 6, 8, 43, 6, 1, 5, 5, 7, 3, 3],
        ),
        true,
    );
    leaf_ext(
        "critical subject alternative name (handled)",
        raw_extension(
            "2.5.29.17",
            true,
            &[0x30, 0x05, 0x82, 0x03, b'a', b'.', b'b'],
        ),
        true,
    );
    // AKID key identifier that does not match the issuer's SKID.
    let akid = |keyid: &[u8]| {
        let mut der = vec![0x30, keyid.len() as u8 + 2, 0x80, keyid.len() as u8];
        der.extend_from_slice(keyid);
        raw_extension("2.5.29.35", false, &der)
    };
    let mismatch = Spec {
        key_ids: false,
        extensions: vec![akid(&[1; 20])],
        ..Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter))
    }
    .build();
    same(
        "AKID mismatch",
        path_verdicts(&mismatch, &[&inter], &root, NOW),
        false,
    );
    let no_ids = Spec {
        key_ids: false,
        ..Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter))
    }
    .build();
    same(
        "no key identifiers",
        path_verdicts(&no_ids, &[&inter], &root, NOW),
        true,
    );
    // A CA certificate as leaf is a valid path (the data key check rejects it).
    let ca_leaf = Spec::ca("LEAFCA", &k.leaf, Some((&inter, &k.inter))).build();
    same(
        "CA leaf path",
        path_verdicts(&ca_leaf, &[&inter], &root, NOW),
        true,
    );
    // V1 leaf.
    let v1_leaf = Spec {
        v1: true,
        ..Spec::leaf("LEAF", &k.leaf, (&inter, &k.inter))
    }
    .build();
    same(
        "V1 leaf",
        path_verdicts(&v1_leaf, &[&inter], &root, NOW),
        true,
    );
    // Explicit EC parameters on the leaf key.
    let mut group = EcGroup::from_curve_name(Nid::SECP384R1).unwrap();
    group.set_asn1_flag(Asn1Flag::EXPLICIT_CURVE);
    let explicit = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let explicit_leaf = Spec::leaf("LEAF", &explicit, (&inter, &k.inter)).build();
    same(
        "explicit EC parameters",
        path_verdicts(&explicit_leaf, &[&inter], &root, NOW),
        false,
    );
}

#[test]
fn rust_backend_fails_closed_where_openssl_semantics_are_not_reproduced() {
    let k = keys();
    let root = Spec::ca("ROOT", &k.root, None).build();
    // Name constraints on a CA (OpenSSL evaluates them; Rust rejects).
    let constrained = Spec {
        extensions: vec![raw_extension(
            "2.5.29.30",
            true,
            &[0x30, 0x07, 0xa0, 0x05, 0x30, 0x03, 0x82, 0x01, b'b'],
        )],
        ..Spec::ca("INTER", &k.inter, Some((&root, &k.root)))
    }
    .build();
    let leaf = Spec::leaf("LEAF", &k.leaf, (&constrained, &k.inter)).build();
    let [openssl, rust] = path_verdicts(&leaf, &[&constrained], &root, NOW);
    assert!(!rust);
    eprintln!("name constraints: OpenSSL {openssl}, Rust {rust}");
    // An intermediate named like the anchor: ambiguous for chain building.
    let twin = Spec {
        key_ids: false,
        ..Spec::ca("ROOT", &k.inter, Some((&root, &k.root)))
    }
    .build();
    let leaf = Spec {
        key_ids: false,
        ..Spec::leaf("LEAF", &k.leaf, (&twin, &k.inter))
    }
    .build();
    let [openssl, rust] = path_verdicts(&leaf, &[&twin], &root, NOW);
    assert!(!rust);
    eprintln!("anchor-named intermediate: OpenSSL {openssl}, Rust {rust}");
    // Case-only name difference (OpenSSL compares canonical names).
    let upper = Spec::ca("INTER", &k.inter, Some((&root, &k.root))).build();
    let lower = Spec::ca("inter", &k.inter2, Some((&upper, &k.inter))).build();
    let leaf = Spec::leaf("LEAF", &k.leaf, (&lower, &k.inter2)).build();
    let [openssl, rust] = path_verdicts(&leaf, &[&lower, &upper], &root, NOW);
    assert!(!rust);
    eprintln!("case-folded names: OpenSSL {openssl}, Rust {rust}");
    // SHA-1 certificate signatures.
    let sha1 = Spec {
        digest: MessageDigest::sha1(),
        ..Spec::leaf("LEAF", &k.leaf, (&root, &k.root))
    }
    .build();
    let [openssl, rust] = path_verdicts(&sha1, &[], &root, NOW);
    assert!(!rust);
    eprintln!("ecdsa-with-SHA1: OpenSSL {openssl}, Rust {rust}");
    // Trailing bytes after a certificate (OpenSSL d2i ignores them).
    let mut trailing = root.to_der().unwrap();
    trailing.push(0);
    assert!(Cert::from_der(CryptoBackend::OpenSsl, &trailing).is_ok());
    assert!(Cert::from_der(CryptoBackend::Rust, &trailing).is_err());
}

// ── Detached payload signatures ───────────────────────────────────────────

/// (case, payload, signature, intermediates, root thumbprint, time, expected)
type DetachedCase<'a> = (
    &'a str,
    &'a [u8],
    &'a [u8],
    Vec<Vec<u8>>,
    Option<&'a str>,
    i64,
    bool,
);

#[test]
fn detached_signatures_give_identical_verdicts() {
    let k = keys();
    let root = Spec::ca("ROOT", &k.root, None).build();
    let top = Spec::ca("TOP", &k.inter2, Some((&root, &k.root))).build();
    let inter = Spec::ca("INTER", &k.inter, Some((&top, &k.inter2))).build();
    let p256 = key(Nid::X9_62_PRIME256V1);
    let data = b"detached payload";
    let pem = root.to_pem().unwrap();
    let der = |c: &X509| c.to_der().unwrap();
    let thumb = hex(&root.digest(MessageDigest::sha256()).unwrap());
    let zeros = "0".repeat(64);
    for (algorithm, leaf_key, digest) in [
        (
            DetachedAlgorithm::Ecdsa384Sha384,
            &k.leaf,
            MessageDigest::sha384(),
        ),
        (
            DetachedAlgorithm::Ecdsa384Sha3,
            &k.leaf,
            MessageDigest::sha3_384(),
        ),
        (
            DetachedAlgorithm::Ecdsa256Sha256,
            &p256,
            MessageDigest::sha256(),
        ),
        (
            DetachedAlgorithm::Ecdsa256Sha3,
            &p256,
            MessageDigest::sha3_256(),
        ),
    ] {
        let leaf = Spec::leaf("LEAF", leaf_key, (&inter, &k.inter)).build();
        let mut signer = Signer::new(digest, leaf_key).unwrap();
        signer.update(data).unwrap();
        let sig = signer.sign_to_vec().unwrap();
        let mut tampered_sig = sig.clone();
        let at = tampered_sig.len() - 3;
        tampered_sig[at] ^= 1;
        let cases: Vec<DetachedCase> = vec![
            (
                "ordered",
                data,
                &sig,
                vec![der(&inter), der(&top)],
                None,
                NOW,
                true,
            ),
            (
                "reversed",
                data,
                &sig,
                vec![der(&top), der(&inter)],
                Some(&thumb),
                NOW,
                true,
            ),
            (
                "extra",
                data,
                &sig,
                vec![der(&top), der(&root), der(&inter)],
                None,
                NOW,
                true,
            ),
            ("missing", data, &sig, vec![der(&inter)], None, NOW, false),
            (
                "tampered data",
                b"other",
                &sig,
                vec![der(&inter), der(&top)],
                None,
                NOW,
                false,
            ),
            (
                "tampered signature",
                data,
                &tampered_sig,
                vec![der(&inter), der(&top)],
                None,
                NOW,
                false,
            ),
            (
                "expired",
                data,
                &sig,
                vec![der(&inter), der(&top)],
                None,
                NOW + 86400,
                false,
            ),
            (
                "thumbprint",
                data,
                &sig,
                vec![der(&inter), der(&top)],
                Some(&zeros),
                NOW,
                false,
            ),
        ];
        for (case, payload, signature, chain, thumbprint, time, expected) in cases {
            let verdicts = BACKENDS.map(|backend| {
                verify_detached_with(
                    backend,
                    payload,
                    signature,
                    &der(&leaf),
                    &chain,
                    &pem,
                    thumbprint,
                    algorithm,
                    time,
                )
                .is_ok()
            });
            same(&format!("detached {case}"), verdicts, expected);
        }
        // Wrong algorithm for the key's curve.
        let wrong = match algorithm {
            DetachedAlgorithm::Ecdsa384Sha384 | DetachedAlgorithm::Ecdsa384Sha3 => {
                DetachedAlgorithm::Ecdsa256Sha256
            }
            _ => DetachedAlgorithm::Ecdsa384Sha384,
        };
        let verdicts = BACKENDS.map(|backend| {
            verify_detached_with(
                backend,
                data,
                &sig,
                &der(&leaf),
                &[der(&inter), der(&top)],
                &pem,
                None,
                wrong,
                NOW,
            )
            .is_ok()
        });
        same("detached wrong curve", verdicts, false);
    }
}

// ── ECDSA signature encodings ─────────────────────────────────────────────

#[test]
fn ecdsa_signature_encodings_match_openssl() {
    let k = key(Nid::SECP384R1);
    let digest = hash(MessageDigest::sha384(), b"payload").unwrap();
    let sig = EcdsaSig::sign(&digest, &k.ec_key().unwrap()).unwrap();
    let canonical = sig.to_der().unwrap();
    let integer = |v: &[u8]| {
        let mut out = vec![2, v.len() as u8];
        out.extend_from_slice(v);
        out
    };
    let sequence = |body: Vec<u8>| {
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out
    };
    let r = sig.r().to_vec();
    let s = sig.s().to_vec();
    let mut encodings: Vec<(String, Vec<u8>)> = vec![("canonical".into(), canonical.clone())];
    let mut trailing = canonical.clone();
    trailing.push(0);
    encodings.push(("trailing".into(), trailing));
    let mut long_form = vec![0x30, 0x81, canonical[1]];
    long_form.extend_from_slice(&canonical[2..]);
    encodings.push(("long-form length".into(), long_form));
    let mut indefinite = vec![0x30, 0x80];
    indefinite.extend_from_slice(&canonical[2..]);
    indefinite.extend_from_slice(&[0, 0]);
    encodings.push(("indefinite length".into(), indefinite));
    let pad = |v: &[u8]| {
        let mut p = vec![0, 0];
        p.extend_from_slice(v);
        p
    };
    encodings.push((
        "leading zeros".into(),
        sequence([integer(&pad(&r)), integer(&s)].concat()),
    ));
    encodings.push((
        "zero r".into(),
        sequence([integer(&[0]), integer(&s)].concat()),
    ));
    encodings.push((
        "empty r".into(),
        sequence([integer(&[]), integer(&s)].concat()),
    ));
    encodings.push((
        "negative r".into(),
        sequence([integer(&[0x80]), integer(&s)].concat()),
    ));
    encodings.push(("one integer".into(), sequence(integer(&r))));
    encodings.push((
        "three integers".into(),
        sequence([integer(&r), integer(&s), integer(&[1])].concat()),
    ));
    encodings.push((
        "octet string".into(),
        sequence([vec![4, 1, 1], integer(&s)].concat()),
    ));
    encodings.push(("empty".into(), Vec::new()));
    for (case, bytes) in &encodings {
        let verdicts = BACKENDS.map(|b| crypto::canonical_ecdsa_der(b, bytes).unwrap_or(false));
        assert_eq!(verdicts[0], verdicts[1], "{case}: {verdicts:?}");
        // Zero is canonically encoded (`02 01 00`); verification rejects it.
        assert_eq!(
            verdicts[0],
            case == "canonical" || case == "zero r",
            "{case}"
        );
    }

    // Verification: valid, high-S (ECDSA malleability, accepted by both),
    // r or s out of range, wrong digest.
    let cert = Spec::leaf("LEAF", &k, (&Spec::ca("ROOT", &k, None).build(), &k)).build();
    let mut ctx = BigNumContext::new().unwrap();
    let mut order = BigNum::new().unwrap();
    EcGroup::from_curve_name(Nid::SECP384R1)
        .unwrap()
        .order(&mut order, &mut ctx)
        .unwrap();
    let mut high_s = BigNum::new().unwrap();
    high_s.checked_sub(&order, sig.s()).unwrap();
    let components = |r: &openssl::bn::BigNumRef, s: &openssl::bn::BigNumRef| {
        EcdsaSig::from_private_components(
            BigNum::from_slice(&r.to_vec()).unwrap(),
            BigNum::from_slice(&s.to_vec()).unwrap(),
        )
        .unwrap()
        .to_der()
        .unwrap()
    };
    let one = BigNum::from_u32(1).unwrap();
    let mut order_plus = BigNum::new().unwrap();
    order_plus.checked_add(&order, &one).unwrap();
    let cases = [
        ("valid", canonical.clone(), digest.to_vec(), true),
        (
            "high s",
            components(sig.r(), &high_s),
            digest.to_vec(),
            true,
        ),
        ("r = n", components(&order, sig.s()), digest.to_vec(), false),
        (
            "s = n + 1",
            components(sig.r(), &order_plus),
            digest.to_vec(),
            false,
        ),
        ("other digest", canonical.clone(), vec![7; 48], false),
    ];
    for (case, signature, digest, expected) in cases {
        let verdicts = BACKENDS.map(|backend| {
            let cert = Cert::from_der(backend, &cert.to_der().unwrap()).unwrap();
            crypto::canonical_ecdsa_der(backend, &signature).unwrap_or(false)
                && cert
                    .verify_ecdsa_digest(&digest, &signature)
                    .unwrap_or(false)
        });
        same(&format!("ECDSA {case}"), verdicts, expected);
    }
}

#[test]
fn trust_anchor_installation_matches_openssl() {
    let k = keys();
    let root = Spec::ca("ROOT", &k.root, None).build();
    let inter = Spec::ca("INTER", &k.inter, Some((&root, &k.root))).build();
    let pem =
        |certs: &[&X509]| -> Vec<u8> { certs.iter().flat_map(|c| c.to_pem().unwrap()).collect() };
    let mut with_text = b"Comment before the certificate\n".to_vec();
    with_text.extend(pem(&[&root]));
    with_text.extend_from_slice(b"\ntrailing text\n");
    let key_block = k.root.private_key_to_pem_pkcs8().unwrap();
    let mut with_key = key_block.clone();
    with_key.extend(pem(&[&root]));
    for (case, input, expected) in [
        ("root", pem(&[&root]), true),
        ("text around", with_text, true),
        ("private key block before", with_key, true),
        ("two certificates", pem(&[&root, &inter]), false),
        ("not self-signed", pem(&[&inter]), false),
        ("no certificate", key_block, false),
        (
            "garbage",
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".to_vec(),
            false,
        ),
    ] {
        let verdicts = BACKENDS.map(|backend| {
            TrustAnchors::with_backend(backend)
                .install_pem("IHO", &input)
                .is_ok()
        });
        same(&format!("anchor {case}"), verdicts, expected);
    }
    // Anchor digests agree.
    let digests: Vec<_> = BACKENDS
        .iter()
        .map(|b| {
            Cert::stack_from_pem(*b, &root.to_pem().unwrap()).unwrap()[0]
                .sha256_hex()
                .unwrap()
        })
        .collect();
    assert_eq!(digests[0], digests[1]);
}

// ── Real exchange sets ────────────────────────────────────────────────────

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn iho_root() -> Option<Vec<u8>> {
    std::fs::read(repository().join("Trust/IHO-S100-5.2.pem")).ok()
}

/// 2026-02-17T10:18:33Z, the UKHO S-124 exchange set time.
const UKHO_ISSUED: i64 = 1_771_323_513;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[test]
fn ukho_s124_exchange_set_verdicts_match() {
    let dir = repository().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT");
    let Some(pem) = iho_root().filter(|_| dir.join("CATALOG.SIGN").exists()) else {
        eprintln!("skipping: UKHO S-124 TestData or IHO trust anchor absent");
        return;
    };
    let report = exchange_parity("UKHO S-124 at issue", &dir, &pem, UKHO_ISSUED).unwrap();
    assert!(report.contains("Certificate Chersoft"));
    let error = exchange_parity("UKHO S-124 now", &dir, &pem, now()).unwrap_err();
    assert!(error.contains("expired"), "{error}");
    // Exactly at the signer's notAfter (2026-06-16T14:43:56Z): expired.
    assert!(exchange_parity("UKHO S-124 at notAfter", &dir, &pem, 1_781_621_036).is_err());
    assert!(exchange_parity("UKHO S-124 before notAfter", &dir, &pem, 1_781_621_035).is_ok());

    // Tampered copies.
    let copy = tempfile::tempdir().unwrap();
    let copy_tree = |from: &Path, to: &Path| {
        for entry in walk(from) {
            let target = to.join(entry.strip_prefix(from).unwrap());
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&entry, target).unwrap();
        }
    };
    copy_tree(&dir, copy.path());
    assert!(exchange_parity("copy", copy.path(), &pem, UKHO_ISSUED).is_ok());
    let dataset = walk(copy.path())
        .into_iter()
        .find(|p| p.extension().is_some_and(|e| e == "GML"))
        .unwrap();
    let original = std::fs::read(&dataset).unwrap();
    let mut tampered = original.clone();
    let at = tampered.len() / 2;
    tampered[at] ^= 0x20;
    std::fs::write(&dataset, &tampered).unwrap();
    assert!(exchange_parity("tampered dataset", copy.path(), &pem, UKHO_ISSUED).is_err());
    std::fs::write(&dataset, &original).unwrap();
    let catalogue = copy.path().join("CATALOG.XML");
    let bytes = std::fs::read(&catalogue).unwrap();
    std::fs::write(&catalogue, [&bytes[..], b" "].concat()).unwrap();
    assert!(exchange_parity("tampered catalogue", copy.path(), &pem, UKHO_ISSUED).is_err());
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Every signed exchange set under TestData (S-101, S-102, S-104, S-111,
/// S-124 from UKHO and SHOM), deduplicated by CATALOG.SIGN, at several times.
#[test]
fn all_signed_test_data_exchange_sets_give_identical_verdicts() {
    let Some(pem) = iho_root() else {
        eprintln!("skipping: IHO trust anchor absent");
        return;
    };
    let data = repository().join("TestData");
    if !data.is_dir() {
        eprintln!("skipping: TestData absent");
        return;
    }
    let mut seen = HashSet::new();
    let mut sets = 0;
    let mut accepted = 0;
    for sign in walk(&data)
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "CATALOG.SIGN"))
    {
        if !seen.insert(
            hash(MessageDigest::sha256(), &std::fs::read(&sign).unwrap())
                .unwrap()
                .to_vec(),
        ) {
            continue;
        }
        sets += 1;
        let root = sign.parent().unwrap();
        // 2026-02-17, 2026-03-26, 2026-05-27, 2026-06-20 and now.
        for time in [
            UKHO_ISSUED,
            1_774_483_200,
            1_779_840_000,
            1_781_913_600,
            now(),
        ] {
            let case = format!("{} t={time}", root.display());
            let verdict = exchange_parity(&case, root, &pem, time);
            accepted += usize::from(verdict.is_ok());
            eprintln!(
                "{case}: {}",
                match &verdict {
                    Ok(_) => "accepted".to_string(),
                    Err(e) => format!("rejected: {e}"),
                }
            );
        }
    }
    eprintln!("{sets} distinct signed exchange sets, {accepted} accepted (set, time) pairs");
    assert!(sets >= 10, "{sets}");
    assert!(accepted > 0);
}

/// The same exchange set read from memory verifies exactly as from files.
#[test]
fn memory_exchange_source_matches_local_files() {
    let dir = repository().join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT");
    let Some(pem) = iho_root().filter(|_| dir.join("CATALOG.SIGN").exists()) else {
        eprintln!("skipping: UKHO S-124 TestData or IHO trust anchor absent");
        return;
    };
    let root = dir.canonicalize().unwrap();
    let mut memory = MemoryExchange::new();
    for file in walk(&root) {
        memory.insert(file.clone(), std::fs::read(&file).unwrap());
    }
    let memory: std::sync::Arc<dyn ExchangeSource> = std::sync::Arc::new(memory);
    for backend in BACKENDS {
        let anchors = anchors_for(&pem, backend);
        let local = verify_exchange(&root, &anchors, UKHO_ISSUED).unwrap();
        let in_memory = verify_exchange_in(&memory, &root, &anchors, UKHO_ISSUED).unwrap();
        assert_eq!(summary(&local), summary(&in_memory));
        let snapshot = in_memory.resources[0].snapshot().unwrap();
        assert_eq!(
            snapshot.bytes().unwrap(),
            std::fs::read(&in_memory.resources[0].path).unwrap()
        );
        assert!(std::fs::read(snapshot.path()).is_err());
    }
}
