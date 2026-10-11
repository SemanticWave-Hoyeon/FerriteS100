use super::*;

#[test]
fn explicit_catalogue_authority_is_scoped_and_default_deny() {
    let f = Fixture::new(true, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let catalogue = verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).unwrap();
    let der = catalogue.signatures()[0].signer_certificate_der();
    let empty = TrustedCancellationPolicy::new("rev-1", vec![]).unwrap();
    assert!(empty
        .authorize_catalogue(&catalogue, "S-102", "FR", "file:/DATA.H5")
        .is_err());
    let grant = TrustedCancellationGrant::from_certificate_der(
        "S-102",
        "FR",
        CancellationDatasetScope::ExactUri("file:/DATA.H5".into()),
        der,
        CancellationAuthorityRole::Producer,
        NOW - 1,
        NOW + 1,
    )
    .unwrap();
    let policy = TrustedCancellationPolicy::new("rev-2", vec![grant]).unwrap();
    let proof = policy
        .authorize_catalogue(&catalogue, "S-102", "FR", "file:/DATA.H5")
        .unwrap();
    assert_eq!(proof.catalogue_sha384(), catalogue.catalogue_sha384());
    assert_eq!(
        proof.signer_certificate_sha256(),
        catalogue.signatures()[0].signer_certificate_sha256()
    );
    assert_eq!(proof.role(), CancellationAuthorityRole::Producer);
    assert_eq!(proof.policy().sha384(), policy.sha384());
    for (product, producer, uri) in [
        ("S-101", "FR", "file:/DATA.H5"),
        ("S-102", "GB", "file:/DATA.H5"),
        ("S-102", "FR", "file:/OTHER.H5"),
        ("S-102", "FR", "file:/data.h5"),
    ] {
        assert!(policy
            .authorize_catalogue(&catalogue, product, producer, uri)
            .is_err());
    }
}

#[test]
fn certificate_subject_public_key_and_xml_id_do_not_grant_authority() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let catalogue = verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).unwrap();
    // Same subject/key, different issuer/certificate: full DER pin must reject.
    let alternate = certificate("PRODUCER", &f.key, None, false, true)
        .to_der()
        .unwrap();
    assert_ne!(
        alternate,
        catalogue.signatures()[0].signer_certificate_der()
    );
    let grant = TrustedCancellationGrant::from_certificate_der(
        "S-102",
        "FR",
        CancellationDatasetScope::AllProducerDatasets,
        &alternate,
        CancellationAuthorityRole::Delegate,
        NOW - 1,
        NOW + 1,
    )
    .unwrap();
    let policy = TrustedCancellationPolicy::new("alternate", vec![grant]).unwrap();
    assert!(policy
        .authorize_catalogue(&catalogue, "S-102", "FR", "file:/DATA.H5")
        .is_err());
}

#[test]
fn authority_windows_delegation_and_policy_audit_identity_are_explicit() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let catalogue = verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).unwrap();
    let der = catalogue.signatures()[0].signer_certificate_der();
    for (start, end, accepted) in [
        (NOW, NOW, true),
        (NOW + 1, NOW + 2, false),
        (NOW - 2, NOW - 1, false),
    ] {
        let grant = TrustedCancellationGrant::from_certificate_der(
            "S-102",
            "FR",
            CancellationDatasetScope::AllProducerDatasets,
            der,
            CancellationAuthorityRole::Delegate,
            start,
            end,
        )
        .unwrap();
        let policy = TrustedCancellationPolicy::new("delegate-1", vec![grant]).unwrap();
        let result = policy.authorize_catalogue(&catalogue, "S-102", "FR", "file:/FUTURE.H5");
        assert_eq!(result.is_ok(), accepted);
        if let Ok(proof) = result {
            assert_eq!(proof.role(), CancellationAuthorityRole::Delegate);
        }
    }
    let make = |revision: &str, role| {
        TrustedCancellationPolicy::new(
            revision,
            vec![TrustedCancellationGrant::from_certificate_der(
                "S-102",
                "FR",
                CancellationDatasetScope::AllProducerDatasets,
                der,
                role,
                NOW,
                NOW,
            )
            .unwrap()],
        )
        .unwrap()
    };
    let a = make("r1", CancellationAuthorityRole::Producer);
    assert_eq!(
        a.sha384(),
        make("r1", CancellationAuthorityRole::Producer).sha384()
    );
    assert_ne!(
        a.sha384(),
        make("r2", CancellationAuthorityRole::Producer).sha384()
    );
    assert_ne!(
        a.sha384(),
        make("r1", CancellationAuthorityRole::Delegate).sha384()
    );
}

#[test]
fn authority_registry_rejects_ambiguous_windows_and_owned_capacity() {
    let f = Fixture::new(false, true, Nid::SECP384R1);
    f.write("file:/DATA.H5", false, "LEAF", "unencrypted");
    let catalogue = verify_exchange_catalogue(f.dir.path(), &f.anchors, NOW).unwrap();
    let der = catalogue.signatures()[0].signer_certificate_der();
    let grant = |scope, start, end| {
        TrustedCancellationGrant::from_certificate_der(
            "S-102",
            "FR",
            scope,
            der,
            CancellationAuthorityRole::Producer,
            start,
            end,
        )
        .unwrap()
    };
    assert!(TrustedCancellationPolicy::new(
        "overlap",
        vec![
            grant(CancellationDatasetScope::AllProducerDatasets, NOW - 1, NOW),
            grant(
                CancellationDatasetScope::ExactUri("file:/DATA.H5".into()),
                NOW,
                NOW + 1
            )
        ]
    )
    .is_err());
    assert!(TrustedCancellationPolicy::new(
        "disjoint",
        vec![
            grant(CancellationDatasetScope::AllProducerDatasets, NOW - 1, NOW),
            grant(
                CancellationDatasetScope::AllProducerDatasets,
                NOW + 1,
                NOW + 2
            )
        ]
    )
    .is_ok());
    let huge =
        Vec::with_capacity(2 * 1024 * 1024 / std::mem::size_of::<TrustedCancellationGrant>() + 1);
    assert!(TrustedCancellationPolicy::new("empty-but-overallocated", huge).is_err());
    let grants = (0..257)
        .map(|n| {
            grant(
                CancellationDatasetScope::ExactUri(format!("file:/D{n}.H5")),
                NOW,
                NOW,
            )
        })
        .collect();
    assert!(TrustedCancellationPolicy::new("too-many", grants).is_err());
    for uri in [
        "file:/../DATA.H5",
        "file:/A/../DATA.H5",
        "file:/A\\DATA.H5",
        "https://example/DATA.H5",
    ] {
        assert!(TrustedCancellationGrant::from_certificate_der(
            "S-102",
            "FR",
            CancellationDatasetScope::ExactUri(uri.into()),
            der,
            CancellationAuthorityRole::Producer,
            NOW,
            NOW
        )
        .is_err());
    }
    let trailing = [der, &[0]].concat();
    assert!(TrustedCancellationGrant::from_certificate_der(
        "S-102",
        "FR",
        CancellationDatasetScope::AllProducerDatasets,
        &trailing,
        CancellationAuthorityRole::Producer,
        NOW,
        NOW
    )
    .is_err());
}

#[test]
fn retained_authority_requires_fresh_local_window_and_rejects_clock_rollback() {
    let fixture = Fixture::new(true, true, Nid::SECP384R1);
    fixture.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let catalogue = verify_exchange_catalogue(fixture.dir.path(), &fixture.anchors, NOW).unwrap();
    let grant = TrustedCancellationGrant::from_certificate_der(
        "S-102",
        "FR",
        CancellationDatasetScope::ExactUri("file:/DATA.H5".into()),
        catalogue.signatures()[0].signer_certificate_der(),
        CancellationAuthorityRole::Producer,
        NOW - 10,
        NOW + 1,
    )
    .unwrap();
    let policy = TrustedCancellationPolicy::new("publication-window", vec![grant]).unwrap();
    let authority = policy
        .authorize_catalogue(&catalogue, "S-102", "FR", "file:/DATA.H5")
        .unwrap();
    assert!(authority.validate_local_window_at(NOW).is_ok());
    assert!(authority.validate_local_window_at(NOW + 1).is_ok());
    assert!(authority.validate_local_window_at(NOW + 2).is_err());
    assert!(authority.validate_local_window_at(NOW - 1).is_err());
}
#[test]
fn old_local_token_does_not_replace_current_policy_reauthorization() {
    let fixture = Fixture::new(true, true, Nid::SECP384R1);
    fixture.write("file:/DATA.H5", true, "LEAF", "unencrypted");
    let catalogue = verify_exchange_catalogue(fixture.dir.path(), &fixture.anchors, NOW).unwrap();
    let grant = TrustedCancellationGrant::from_certificate_der(
        "S-102",
        "FR",
        CancellationDatasetScope::AllProducerDatasets,
        catalogue.signatures()[0].signer_certificate_der(),
        CancellationAuthorityRole::Delegate,
        NOW,
        NOW + 10,
    )
    .unwrap();
    let old = TrustedCancellationPolicy::new("old-admin", vec![grant]).unwrap();
    let current = TrustedCancellationPolicy::new("grant-revoked", vec![]).unwrap();
    let authority = old
        .authorize_catalogue(&catalogue, "S-102", "FR", "file:/DATA.H5")
        .unwrap();
    assert!(authority.validate_local_window_at(NOW + 1).is_ok());
    assert!(current
        .authorize_catalogue(&catalogue, "S-102", "FR", "file:/DATA.H5")
        .is_err());
}
