//! Synthetic catalogue authentication with actual original metadata/signatures.
//! Local explicit synthetic delegate grant tests only; no real producer/delegation,
//! removal permission or official cancellation notice claim. Historical receipt
//! persistence/reopen checks do not activate App replay enforcement.
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ferrite_s102::discovery::{
    AuthenticatedS102OriginalMetadata, S102AuthorizedCancellation, S102CancellationEvidence,
    S102CancellationMetadata,
};
use ferrite_security::{
    capture_catalogue_authenticated_incoming_exchange, verify_exchange, CancellationAuthorityRole,
    CancellationDatasetScope, DatasetDiscoveryAuthorization, IncomingExchangeLimits, TrustAnchors,
    TrustedCancellationGrant, TrustedCancellationPolicy,
};
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
use std::{fs, path::Path};
const XC: &str = "http://www.iho.int/s100/xc/5.2";
const SE: &str = "http://www.iho.int/s100/se/5.2";
fn key() -> Result<PKey<Private>> {
    let group = EcGroup::from_curve_name(Nid::SECP384R1)?;
    Ok(PKey::from_ec_key(EcKey::generate(&group)?)?)
}
fn cert(
    name: &str,
    k: &PKey<Private>,
    issuer: Option<(&X509, &PKey<Private>)>,
    epoch: i64,
) -> Result<X509> {
    let mut n = X509NameBuilder::new()?;
    n.append_entry_by_text("CN", name)?;
    let n = n.build();
    let mut b = X509::builder()?;
    b.set_version(2)?;
    let serial = BigNum::from_u32(if issuer.is_none() { 1 } else { 2 })?.to_asn1_integer()?;
    b.set_serial_number(&serial)?;
    b.set_subject_name(&n)?;
    b.set_issuer_name(issuer.map(|x| x.0.subject_name()).unwrap_or(&n))?;
    b.set_pubkey(k)?;
    let before = Asn1Time::from_unix(epoch.checked_sub(3600).context("epoch underflow")?)?;
    let after = Asn1Time::from_unix(epoch.checked_add(86400).context("epoch overflow")?)?;
    b.set_not_before(&before)?;
    b.set_not_after(&after)?;
    let mut bc = BasicConstraints::new();
    bc.critical();
    if issuer.is_none() {
        bc.ca();
    }
    b.append_extension(bc.build()?)?;
    let mut usage = KeyUsage::new();
    usage.critical();
    if issuer.is_none() {
        usage.key_cert_sign().crl_sign();
    } else {
        usage.digital_signature();
    }
    b.append_extension(usage.build()?)?;
    b.sign(issuer.map(|x| x.1).unwrap_or(k), MessageDigest::sha384())?;
    Ok(b.build())
}
fn sign(k: &PKey<Private>, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut s = Signer::new(MessageDigest::sha384(), k)?;
    s.update(bytes)?;
    Ok(s.sign_to_vec()?)
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}
fn write_signed(dir: &Path, xml: &str, k: &PKey<Private>, container: &str) -> Result<()> {
    fs::write(dir.join("CATALOG.XML"), xml.as_bytes())?;
    let sig=format!("<se:StandaloneDigitalSignature xmlns:se=\"{SE}\"><se:filename>CATALOG.XML</se:filename><se:certificates>{container}</se:certificates><se:digitalSignature id=\"CAT\" certificateRef=\"TESTLEAF\">{}</se:digitalSignature></se:StandaloneDigitalSignature>",STANDARD.encode(sign(k,xml.as_bytes())?));
    fs::write(dir.join("CATALOG.SIGN"), sig)?;
    Ok(())
}
fn require_original(
    d: &DatasetDiscoveryAuthorization,
) -> Result<AuthenticatedS102OriginalMetadata> {
    match d {
        DatasetDiscoveryAuthorization::Authenticated(a) => {
            AuthenticatedS102OriginalMetadata::from_authentication(a.original_authentication())
        }
        _ => bail!("Signature-OFF/evaluation has no retained original proof"),
    }
}
fn main() -> Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|v| v == "--journal-store-check") {
        ensure!(args.len() == 5, "mode journal expected-codec required");
        use ferrite_s102::cancellation_journal::store::Store;
        match args[2].as_str() {
            "locked" => ensure!(
                Store::open(Path::new(&args[3])).is_err(),
                "Independent process obtained a concurrently held journal"
            ),
            "reopen" => {
                let store = Store::open(Path::new(&args[3]))?;
                let expected = ferrite_s102::cancellation_journal::Journal::read_from(
                    fs::File::open(&args[4])?,
                )?;
                ensure!(
                    store.journal()? == &expected && expected.len() == 1,
                    "Independent process did not retain exact authenticated receipt"
                );
            }
            _ => bail!("Unknown journal probe mode"),
        }
        println!("CHECK\tindependent_process_{}\tPASS", args[2]);
        return Ok(());
    }
    ensure!(
        args.len() == 4 || (args.len() == 6 && args[4] == "--export-test-history"),
        "actualS100root IHOanchor verification_epoch [--export-test-history absolute-new-file] required"
    );
    let export_history = args.get(5).map(std::path::PathBuf::from);
    if let Some(path) = &export_history {
        ensure!(
            path.is_absolute() && !path.exists(),
            "Test history export needs an absolute new file; no overwrite"
        );
        ensure!(
            path.parent().is_some_and(Path::is_dir),
            "Test history parent must already exist"
        );
    }
    let epoch: i64 = args[3].parse()?;
    let mut iho = TrustAnchors::default();
    iho.install_pem("IHO", &fs::read(&args[2])?)?;
    let verified = verify_exchange(&args[1], &iho, epoch)?;
    ensure!(
        verified.dataset_discovery.len() == 7,
        "Expected seven actual signed SHOM originals"
    );
    let proof = verified
        .dataset_discovery
        .iter()
        .min_by_key(|(p, _)| *p)
        .context("No actual original")?
        .1
        .original_authentication();
    let original = AuthenticatedS102OriginalMetadata::from_authentication(proof)?;
    let catalogue_before = proof.catalogue_bytes().to_vec();
    let resource_before = proof.resource_sha384().to_owned();
    let original_snapshot = std::sync::Arc::new(proof.capture_resource()?);
    ensure!(
        original_snapshot.path().is_file(),
        "Retained original snapshot missing"
    );
    let view = proof.original_entry_view()?;
    let node = view.entry();
    let range = node.range();
    let raw = std::str::from_utf8(proof.catalogue_bytes())?;
    let old_entry = &raw[range.clone()];
    let prefix = node
        .lookup_prefix(XC)
        .context("Actual XC prefix required by this authored probe")?;
    let child = |name: &str| {
        node.children()
            .find(|n| {
                n.is_element()
                    && n.tag_name().namespace() == Some(XC)
                    && n.tag_name().name() == name
            })
            .context(format!("Missing {name}"))
    };
    let olddate = chrono::NaiveDate::parse_from_str(
        child("issueDate")?.text().context("Missing date")?.trim(),
        "%Y-%m-%d",
    )?;
    let nextdate = olddate.succ_opt().context("Date overflow")?;
    let mut patches = vec![];
    for (name, value) in [
        ("purpose", "cancellation".to_owned()),
        ("issueDate", nextdate.to_string()),
    ] {
        let n = child(name)?;
        patches.push((
            n.range().start - range.start,
            n.range().end - range.start,
            format!("<{prefix}:{name}>{value}</{prefix}:{name}>"),
        ));
    }
    for n in node.children().filter(|n| {
        n.is_element()
            && n.tag_name().namespace() == Some(XC)
            && ["issueTime", "replacedData", "dataReplacement"].contains(&n.tag_name().name())
    }) {
        patches.push((
            n.range().start - range.start,
            n.range().end - range.start,
            String::new(),
        ));
    }
    patches.sort_by_key(|p| std::cmp::Reverse(p.0));
    let mut entry = old_entry.to_owned();
    for (a, b, x) in patches {
        entry.replace_range(a..b, &x);
    }
    let close = entry.rfind("</").context("Missing entry closing tag")?;
    entry.insert_str(close,&format!("<{prefix}:issueTime>18:30:59Z</{prefix}:issueTime><{prefix}:replacedData>false</{prefix}:replacedData>"));
    let namespaces = node
        .namespaces()
        .map(|ns| match ns.name() {
            Some(p) => format!(" xmlns:{p}=\"{}\"", esc(ns.uri())),
            None => format!(" xmlns=\"{}\"", esc(ns.uri())),
        })
        .collect::<String>();
    let cert_node = node
        .document()
        .root_element()
        .children()
        .find(|n| {
            n.is_element()
                && n.tag_name().namespace() == Some(XC)
                && n.tag_name().name() == "certificates"
        })
        .context("Actual original certificates required")?;
    let original_certificates = &raw[cert_node.range()];
    let wrap = |e: &str| {
        format!("<{prefix}:S100_ExchangeCatalogue{namespaces}><{prefix}:datasetDiscoveryMetadata>{e}</{prefix}:datasetDiscoveryMetadata>{original_certificates}</{prefix}:S100_ExchangeCatalogue>")
    };
    let notice = wrap(&entry);
    let rk = key()?;
    let root = cert("SYNTHETIC TEST ROOT ONLY", &rk, None, epoch)?;
    let lk = key()?;
    let leaf = cert(
        "UNRELATED SYNTHETIC TEST SIGNER",
        &lk,
        Some((&root, &rk)),
        epoch,
    )?;
    let container=format!("<se:schemeAdministrator id=\"SYNTHETIC\"/><se:certificate id=\"TESTLEAF\" issuer=\"SYNTHETIC\">{}</se:certificate>",STANDARD.encode(leaf.to_der()?));
    let mut anchors = TrustAnchors::default();
    anchors.install_pem("SYNTHETIC", &root.to_pem()?)?;
    // Keys never serialized. All fixture XML/public certs/signatures live in RAII TempDir.
    let dir = tempfile::tempdir()?;
    write_signed(dir.path(), &notice, &lk, &container)?;
    let owned = capture_catalogue_authenticated_incoming_exchange(
        dir.path(),
        &anchors,
        epoch,
        IncomingExchangeLimits::default(),
    )?;
    ensure!(
        owned.file_count() == 2 && owned.resource_bytes(original.resource_uri())?.is_none(),
        "Notice is not catalogue-only"
    );
    let parsed = S102CancellationMetadata::from_absence(
        owned.prove_ascii_resource_absent(original.resource_uri())?,
    )?;
    parsed.validate_original_nonsignature_metadata(&original)?;
    ensure!(
        parsed.resource_uri() == original.resource_uri()
            && parsed.producer_code() == original.producer_code(),
        "Metadata mismatch"
    );
    println!("CHECK\tpositive_signed_catalogue_only_metadata\tPASS");
    let signatures =
        proof.bind_copied_signatures_from_snapshot(owned.catalogue(), original_snapshot.clone())?;
    let admitted_metadata = S102CancellationMetadata::from_absence(
        owned.prove_ascii_resource_absent(original.resource_uri())?,
    )?;
    let evidence = S102CancellationEvidence::bind(admitted_metadata, signatures)?;
    ensure!(
        std::ptr::eq(
            evidence.original().resource_snapshot(),
            original_snapshot.as_ref()
        ),
        "Original payload was copied instead of retained"
    );
    println!("CHECK\towned_absence_metadata_original_signature_conjunction\tPASS");
    let foreign = capture_catalogue_authenticated_incoming_exchange(
        dir.path(),
        &anchors,
        epoch,
        IncomingExchangeLimits::default(),
    )?;
    let foreign_signatures = proof
        .bind_copied_signatures_from_snapshot(foreign.catalogue(), original_snapshot.clone())?;
    let same_metadata = S102CancellationMetadata::from_absence(
        owned.prove_ascii_resource_absent(original.resource_uri())?,
    )?;
    ensure!(
        S102CancellationEvidence::bind(same_metadata, foreign_signatures).is_err(),
        "Different owned receiver namespaces were mixed"
    );
    println!("CHECK\tforeign_owned_namespace_rejected\tPASS");

    let make_evidence = || -> Result<S102CancellationEvidence<'_>> {
        let signatures = proof
            .bind_copied_signatures_from_snapshot(owned.catalogue(), original_snapshot.clone())?;
        let metadata = S102CancellationMetadata::from_absence(
            owned.prove_ascii_resource_absent(original.resource_uri())?,
        )?;
        S102CancellationEvidence::bind(metadata, signatures)
    };
    let empty_policy = TrustedCancellationPolicy::new("synthetic-empty", vec![])?;
    ensure!(
        S102AuthorizedCancellation::authorize(make_evidence()?, &empty_policy).is_err(),
        "PKI membership incorrectly supplied producer authority"
    );
    println!("CHECK\ttrusted_catalogue_without_explicit_authority_rejected\tPASS");
    let original_grant = TrustedCancellationGrant::from_certificate_der(
        "S-102",
        original.producer_code(),
        CancellationDatasetScope::ExactUri(original.resource_uri().into()),
        proof.signatures()[0].signer_certificate_der(),
        CancellationAuthorityRole::Producer,
        epoch,
        epoch,
    )?;
    let original_policy =
        TrustedCancellationPolicy::new("original-producer-pin-only", vec![original_grant])?;
    ensure!(
        S102AuthorizedCancellation::authorize(make_evidence()?, &original_policy).is_err(),
        "Copied original signer incorrectly authorized an unrelated catalogue signer"
    );
    println!("CHECK\tcopied_original_signer_does_not_authorize_notice\tPASS");
    // This grant models an independent local administrator decision in a test.
    // It does not claim the original producer actually delegated to this signer.
    let synthetic_grant = TrustedCancellationGrant::from_certificate_der(
        "S-102",
        original.producer_code(),
        CancellationDatasetScope::ExactUri(original.resource_uri().into()),
        &leaf.to_der()?,
        CancellationAuthorityRole::Delegate,
        epoch,
        epoch,
    )?;
    let synthetic_policy =
        TrustedCancellationPolicy::new("explicit-synthetic-delegate-only", vec![synthetic_grant])?;
    let authorized = S102AuthorizedCancellation::authorize(make_evidence()?, &synthetic_policy)?;
    ensure!(
        authorized.authority().role() == CancellationAuthorityRole::Delegate
            && authorized.authority().resource_uri() == original.resource_uri()
            && authorized.authority().producer_code() == original.producer_code()
            && authorized.authority().catalogue_sha384() == owned.catalogue().catalogue_sha384()
            && std::ptr::eq(
                authorized.evidence().original().resource_snapshot(),
                original_snapshot.as_ref()
            ),
        "Explicit scoped authority lost catalogue/metadata/snapshot binding"
    );
    println!("CHECK\texplicit_synthetic_delegate_authority_conjunction\tPASS");
    let current = authorized.revalidate_local_authority(
        proof,
        original_snapshot.as_ref(),
        &synthetic_policy,
        epoch,
    )?;
    ensure!(
        current.policy().sha384() == synthetic_policy.sha384(),
        "Current policy not pinned"
    );
    println!("CHECK\tcurrent_local_authority_revalidated\tPASS");
    ensure!(
        authorized
            .revalidate_local_authority(
                proof,
                original_snapshot.as_ref(),
                &synthetic_policy,
                epoch + 1
            )
            .is_err()
            && authorized
                .revalidate_local_authority(
                    proof,
                    original_snapshot.as_ref(),
                    &synthetic_policy,
                    epoch - 1
                )
                .is_err(),
        "Expired grant or clock rollback accepted"
    );
    println!("CHECK\texpired_grant_and_clock_rollback_rejected\tPASS");
    ensure!(
        authorized
            .revalidate_local_authority(proof, original_snapshot.as_ref(), &empty_policy, epoch)
            .is_err(),
        "Revoked current policy reused old authority"
    );
    println!("CHECK\trevoked_current_policy_rejected\tPASS");
    let cloned = proof.clone();
    authorized.revalidate_local_authority(
        &cloned,
        original_snapshot.as_ref(),
        &synthetic_policy,
        epoch,
    )?;
    let fresh_verification = verify_exchange(&args[1], &iho, epoch)?;
    let replaced = fresh_verification
        .dataset_discovery
        .iter()
        .min_by_key(|(p, _)| *p)
        .context("No reverified original")?
        .1
        .original_authentication();
    ensure!(
        authorized
            .revalidate_local_authority(
                replaced,
                original_snapshot.as_ref(),
                &synthetic_policy,
                epoch
            )
            .is_err(),
        "Identical-content replacement owner reused original authority"
    );
    let foreign_snapshot = proof.capture_resource()?;
    ensure!(
        authorized
            .revalidate_local_authority(proof, &foreign_snapshot, &synthetic_policy, epoch)
            .is_err(),
        "Independent parser snapshot reused original authority"
    );
    println!("CHECK\treplaced_original_owner_rejected\tPASS");
    let receipt = ferrite_s102::cancellation_journal::Receipt::from_authorized_notice(
        &authorized,
        epoch.try_into()?,
    )?;
    let target = ferrite_s102::cancellation_journal::TargetKey::from_authenticated_original(proof)?;
    let restarted = ferrite_s102::cancellation_journal::Journal::decode(
        &ferrite_s102::cancellation_journal::Journal::default()
            .stage_receipt(receipt.clone())?
            .encode()?,
    )?;
    ensure!(
        restarted.contains(&target) && restarted.stage_receipt(receipt.clone()).is_err(),
        "Authenticated receipt roundtrip admitted duplicate original"
    );
    ensure!(
        target
            == ferrite_s102::cancellation_journal::TargetKey::from_authenticated_original(
                replaced
            )?,
        "Reverification changed logical original key"
    );
    let keys = fresh_verification
        .dataset_discovery
        .values()
        .map(|v| {
            ferrite_s102::cancellation_journal::TargetKey::from_authenticated_original(
                v.original_authentication(),
            )
        })
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    ensure!(
        keys.len() == 7,
        "Actual seven signed originals did not yield distinct journal keys"
    );
    println!("CHECK\tauthenticated_receipt_codec_and_duplicate_original\tPASS");

    // Use an independent private directory: adding journal fixtures must not
    // change the authenticated incoming exchange's owned absence evidence.
    let history_dir = tempfile::tempdir()?;
    let history_path = history_dir.path().join("cancellation.log");
    let expected_path = history_dir.path().join("expected.codec");
    fs::write(&expected_path, restarted.encode()?)?;
    use ferrite_s102::cancellation_journal::store::{CommitOutcome, Store};
    let mut store = Store::initialize(&history_path)?;
    ensure!(
        matches!(store.persist(receipt.clone()), CommitOutcome::Committed(_)),
        "Authenticated receipt was not committed to disk"
    );
    let check_process = |mode: &str| -> Result<()> {
        let output = std::process::Command::new(std::env::current_exe()?)
            .arg("--journal-store-check")
            .arg(mode)
            .arg(&history_path)
            .arg(&expected_path)
            .output()?;
        ensure!(
            output.status.success(),
            "Journal child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    check_process("locked")?;
    drop(store);
    check_process("reopen")?;
    let mut store = Store::open(&history_path)?;
    ensure!(
        store.journal()?.contains(&target)
            && matches!(store.persist(receipt.clone()), CommitOutcome::Unchanged(_)),
        "Reopened authenticated receipt admitted a duplicate"
    );
    println!("CHECK\tauthenticated_receipt_disk_process_restart_and_duplicate\tPASS");

    ensure!(
        capture_catalogue_authenticated_incoming_exchange(
            dir.path(),
            &TrustAnchors::default(),
            epoch,
            IncomingExchangeLimits::default()
        )
        .is_err(),
        "Missing catalogue trust accepted"
    );
    ensure!(
        require_original(&DatasetDiscoveryAuthorization::SignatureVerificationDisabled).is_err(),
        "OFF manufactured original proof"
    );
    ensure!(
        require_original(&DatasetDiscoveryAuthorization::UnsignedEvaluation).is_err(),
        "Evaluation manufactured original proof"
    );
    println!("CHECK\tmissing_trust_signature_off_proof\tPASS");
    let payload = dir.path().join(
        original
            .resource_uri()
            .strip_prefix("file:/")
            .context("URI")?,
    );
    fs::create_dir_all(payload.parent().context("parent")?)?;
    fs::write(&payload, b"not an authenticated dataset")?;
    let present = capture_catalogue_authenticated_incoming_exchange(
        dir.path(),
        &anchors,
        epoch,
        IncomingExchangeLimits::default(),
    )?;
    ensure!(
        present
            .prove_ascii_resource_absent(original.resource_uri())
            .is_err(),
        "Physical presence accepted as absence"
    );
    ensure!(
        owned
            .prove_ascii_resource_absent(original.resource_uri())
            .is_ok(),
        "Owned captured namespace changed"
    );
    fs::remove_file(&payload)?;
    println!("CHECK\tphysical_presence_owned_absence\tPASS");
    // Fresh correctly signed negative metadata variants, not mere cryptographic failures.
    for (name, xml) in [
        (
            "wrong_product",
            notice.replace(">Bathymetric Surface<", ">Incorrect Product<"),
        ),
        ("s101_product", notice.replace(">S-102<", ">S-101<")),
        (
            "replacement_inconsistent",
            notice.replace(
                &format!("<{prefix}:replacedData>false"),
                &format!("<{prefix}:replacedData>true"),
            ),
        ),
    ] {
        ensure!(xml != notice, "Negative fixture mutation not applied");
        write_signed(dir.path(), &xml, &lk, &container)?;
        let bad = capture_catalogue_authenticated_incoming_exchange(
            dir.path(),
            &anchors,
            epoch,
            IncomingExchangeLimits::default(),
        )?;
        ensure!(
            S102CancellationMetadata::from_absence(
                bad.prove_ascii_resource_absent(original.resource_uri())?
            )
            .is_err(),
            "Invalid metadata accepted: {name}"
        );
        println!("CHECK\t{name}\tPASS");
    }
    let edition: u32 = child("editionNumber")?
        .text()
        .context("Missing edition")?
        .trim()
        .parse()?;
    let replacement = format!(
        "<{prefix}:editionNumber>{}</{prefix}:editionNumber>",
        edition.checked_add(1).context("Edition overflow")?
    );
    let changed = notice.replace(&raw[child("editionNumber")?.range()], &replacement);
    ensure!(changed != notice, "Edition mutation missing");
    write_signed(dir.path(), &changed, &lk, &container)?;
    let changed_owned = capture_catalogue_authenticated_incoming_exchange(
        dir.path(),
        &anchors,
        epoch,
        IncomingExchangeLimits::default(),
    )?;
    let changed_meta = S102CancellationMetadata::from_absence(
        changed_owned.prove_ascii_resource_absent(original.resource_uri())?,
    )?;
    ensure!(
        changed_meta
            .validate_original_nonsignature_metadata(&original)
            .is_err(),
        "Original mandatory edition mismatch accepted"
    );
    println!("CHECK\tmandatory_original_mismatch\tPASS");
    write_signed(dir.path(), &notice, &lk, &container)?;
    fs::write(dir.path().join("CATALOG.XML"), format!("{notice} "))?;
    ensure!(
        capture_catalogue_authenticated_incoming_exchange(
            dir.path(),
            &anchors,
            epoch,
            IncomingExchangeLimits::default()
        )
        .is_err(),
        "CAT tamper accepted"
    );
    let retained = S102CancellationMetadata::from_absence(
        owned.prove_ascii_resource_absent(original.resource_uri())?,
    )?;
    retained.validate_original_nonsignature_metadata(&original)?;
    ensure!(
        proof.catalogue_bytes() == catalogue_before && proof.resource_sha384() == resource_before,
        "Original retained proof changed"
    );
    let again = proof.capture_resource()?;
    ensure!(again.path().is_file(), "Original resource changed");
    println!("CHECK\tcat_tamper_retained_original_namespace\tPASS");
    // Explicit test artifact only, after every preceding authority/metadata
    // negative passed. No keys or grants are serialized; never an official
    // producer notice or a production App cancellation receiver.
    if let Some(path) = export_history {
        let mut exported = Store::initialize(&path)?;
        ensure!(
            matches!(exported.persist(receipt), CommitOutcome::Committed(_)),
            "Explicit test history export did not durably commit"
        );
        ensure!(
            exported.journal()?.validate_reimport(proof).is_err(),
            "Exported tombstone did not reject exact original"
        );
        println!("TEST_HISTORY\t{}\t{}\t{:?}\tactual signed original + synthetic notice/delegate policy only",
            original.resource_uri(), resource_before, path);
    }
    println!("SUMMARY\t17\t{}\t{}\tsynthetic catalogue authentication + actual original metadata/signatures + owned absence + explicit synthetic local delegate policy; NO real producer delegation/replay/removal/official cancellation certification",original.resource_uri(),owned.namespace_sha384());
    Ok(())
}
