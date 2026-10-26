//! Real UKHO 2026 S-124 exchange set: CATALOG.XML lists an extra, unreferenced
//! "Chersoft" certificate outside the IHO scheme; CATALOG.SIGN labels the GB00
//! issuer with the IHO root MRN. Skipped when TestData or Trust is absent.
use super::*;

/// S100XC dateTime of the exchange set (2026-02-17T10:18:33Z), within the
/// GB00 producer certificate validity (2025-06-16 .. 2026-06-16).
const ISSUED: i64 = 1_771_323_513;

fn fixture() -> Option<(PathBuf, TrustAnchors)> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = base.join("TestData/UKHO-S100-2026/ExchangeSets/S-124_GB/S100_ROOT");
    let root = base.join("Trust/IHO-S100-5.2.pem");
    if !dir.join("CATALOG.SIGN").exists() || !root.exists() {
        eprintln!("skipping: UKHO S-124 TestData or IHO trust anchor absent");
        return None;
    }
    let mut anchors = TrustAnchors::default();
    anchors
        .install_pem("IHO", &std::fs::read(root).unwrap())
        .unwrap();
    Some((dir, anchors))
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Cryptographic half of verify_exchange over the real set: catalogue
/// signature, then every dataset signature with only referenced certificates.
/// Returns (verified resource count, certificate warnings).
fn verify_signatures(
    dir: &Path,
    anchors: &TrustAnchors,
    time: i64,
) -> Result<(usize, Vec<String>)> {
    let root = dir.canonicalize()?;
    let authenticated = verify_exchange_catalogue(&root, anchors, time)?;
    let document = xml(authenticated.catalogue_bytes())?;
    let node = document.root_element();
    let mut pending = Vec::new();
    for entry in node
        .descendants()
        .filter(|n| is(*n, XC, "S100_DatasetDiscoveryMetadata"))
    {
        let path = resource_path(
            &LocalFiles,
            &root,
            text(unique_child(entry, XC, "fileName")?)?,
        )?;
        let signatures = entry
            .children()
            .filter(|n| is(*n, XC, "digitalSignatureValue"))
            .flat_map(|c| c.children())
            .filter(Node::is_element)
            .map(parse_signature)
            .collect::<Result<Vec<_>>>()?;
        pending.push((path, signatures));
    }
    let mut warnings = authenticated.certificate_warnings().to_vec();
    let certs = Certificates::parse(node, anchors.backend())?.verified_referenced(
        pending
            .iter()
            .flat_map(|(_, s)| s.iter().map(|s| s.certificate.as_str())),
        anchors,
        time,
        &mut warnings,
    )?;
    ensure!(
        !certs.contains_key("Chersoft"),
        "Unreferenced certificate became usable"
    );
    let count = pending.len();
    for (path, signatures) in pending {
        verify_resource(&SourceHandle(local_files()), path, signatures, &certs)?;
    }
    Ok((count, warnings))
}

#[test]
fn ukho_s124_signatures_verify_end_to_end() {
    let Some((dir, anchors)) = fixture() else {
        return;
    };
    let (count, warnings) = verify_signatures(&dir, &anchors, ISSUED).unwrap();
    assert_eq!(count, 5);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("Certificate Chersoft") && warnings[0].contains("ignored"));
}

#[test]
fn ukho_s124_full_admission_passes_with_missing_issue_date_warnings() {
    // All signatures and the GB00 chain verify at issue time; the UKHO
    // catalogue omits the S100XC issueDate, which is reported as a warning.
    let Some((dir, anchors)) = fixture() else {
        return;
    };
    let report = verify_exchange(&dir, &anchors, ISSUED).unwrap();
    assert_eq!(report.dataset_discovery.len(), 5);
    assert!(report
        .dataset_discovery
        .values()
        .all(|d| d.discovery().issue_date.is_none()));
    let missing = report
        .metadata_warnings
        .iter()
        .filter(|w| w.starts_with("issueDate missing for "))
        .count();
    assert_eq!(missing, 5, "{:?}", report.metadata_warnings);
    assert!(report
        .metadata_warnings
        .iter()
        .any(|w| w.contains("Certificate Chersoft") && w.contains("ignored")));
}

#[test]
fn ukho_s124_full_admission_now_fails_only_on_signer_expiry() {
    let Some((dir, anchors)) = fixture() else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // GB00:00540 notAfter is 2026-06-16T14:43:56Z.
    assert!(now > 1_781_621_036, "test assumes the signer has expired");
    let error = verify_exchange(&dir, &anchors, now).unwrap_err();
    assert!(error.to_string().contains("expired"), "{error:#}");
    assert!(!format!("{error:#}").contains("issueDate"), "{error:#}");
}

#[test]
fn ukho_s124_producer_certificate_expiry_is_still_enforced() {
    let Some((dir, anchors)) = fixture() else {
        return;
    };
    // GB00:00540 notAfter is 2026-06-16T14:43:56Z.
    let error = verify_signatures(&dir, &anchors, 1_781_621_037).unwrap_err();
    assert!(error.to_string().contains("expired"), "{error:#}");
}

#[test]
fn ukho_s124_tampered_dataset_fails() {
    let Some((dir, anchors)) = fixture() else {
        return;
    };
    let copy = tempfile::tempdir().unwrap();
    copy_tree(&dir, copy.path());
    let dataset = copy
        .path()
        .join("S-124/DATASET_FILES/124GB00S2Y2026N44.GML");
    let mut bytes = std::fs::read(&dataset).unwrap();
    let last = bytes.len() - 2;
    bytes[last] ^= 0x01;
    std::fs::write(&dataset, bytes).unwrap();
    let error = verify_signatures(copy.path(), &anchors, ISSUED).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Invalid data signature 124GB00S2Y2026N44"),
        "{error:#}"
    );
}

#[test]
fn ukho_s124_tampered_catalogue_fails() {
    let Some((dir, anchors)) = fixture() else {
        return;
    };
    let copy = tempfile::tempdir().unwrap();
    copy_tree(&dir, copy.path());
    let catalogue = copy.path().join("CATALOG.XML");
    let text = std::fs::read_to_string(&catalogue).unwrap();
    std::fs::write(
        &catalogue,
        text.replace("UK Hydrographic Office", "UK Hydrographic Offlce"),
    )
    .unwrap();
    let error = verify_signatures(copy.path(), &anchors, ISSUED).unwrap_err();
    assert!(
        error.to_string().contains("Invalid data signature catalog"),
        "{error:#}"
    );
}
