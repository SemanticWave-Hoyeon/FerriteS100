//! CPU only. Usage: <FC.xml> <PC-root> <new-output-dir> <base> [ordered updates...]
//! Compare engine-local and process-shared compiler retention with identical
//! retained inputs, fresh VMs, and complete ordered typed portrayal outputs.
use anyhow::{ensure, Context, Result};
use ferrite_feature_catalog::{BoundFeatureCatalogue, FeatureCatalogue};
use ferrite_lua::{
    ChunkCache, ContextParameters, PortrayalContext, PortrayalEngine, PortrayalResult,
    TypeCatalogue,
};
use ferrite_portrayal_catalog::{BoundPortrayalCatalogue, CatalogueSources, PortrayalCatalogue};
use ferrite_s100_core::S101Cell;
use ferrite_security::UnauthenticatedSnapshot;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}
fn file_digest(path: &Path) -> Result<(String, u64)> {
    let mut input = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size = size.checked_add(n as u64).context("Size overflow")?;
        ensure!(size <= 512 * 1024 * 1024, "Input exceeds 512 MiB");
        hash.update(&buf[..n]);
    }
    Ok((hex(&hash.finalize()), size))
}
fn canonical(results: &[PortrayalResult]) -> Vec<u8> {
    // Full typed tree, including command order, all f64/f32 debug values,
    // visibility, observed parameters and feature IDs. No digest-only equality.
    // Public result tree contains ordered vectors, not unordered maps. This is
    // a same-binary differential representation, not a persisted wire format.
    format!("{results:#?}").into_bytes()
}
fn engine(
    sources: Arc<CatalogueSources>,
    fc: &BoundFeatureCatalogue,
    shared: bool,
) -> Result<PortrayalEngine> {
    let cache = if shared {
        ChunkCache::process_shared()
    } else {
        ChunkCache::default()
    };
    let mut engine = PortrayalEngine::new_with_sources_and_chunk_cache(sources, cache)?;
    engine.set_type_catalogue(TypeCatalogue::from_feature_catalogue(fc));
    engine.initialize()?;
    Ok(engine)
}
fn compare(
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    cell: &S101Cell,
    sources: Arc<CatalogueSources>,
    ctx: ContextParameters,
    label: &str,
    out: &Path,
) -> Result<serde_json::Value> {
    let portrayal = PortrayalContext::from_cell(cell, ctx.clone());
    let data = portrayal.cell_data();
    let feature_input_change = if label == "feature-input-change" {
        let mut changed = data.write().unwrap();
        let mut ids: Vec<_> = changed.features.keys().copied().collect();
        ids.sort_unstable();
        let mut evidence = None;
        for id in ids {
            let f = changed.features.get_mut(&id).unwrap();
            if !matches!(
                f.code.as_str(),
                "Wreck" | "Obstruction" | "UnderwaterAwashRock"
            ) {
                continue;
            }
            if let Some(old) = f.attributes.get("valueOfSounding") {
                if let Ok(value) = old.as_string().parse::<f64>() {
                    if value.is_finite() {
                        let updated = value + 1000.;
                        evidence = Some(
                            serde_json::json!({"feature_id":id,"attribute":"valueOfSounding","before":format!("{old:?}"),"after":updated,"scope":"derived host fixture only; producer data untouched"}),
                        );
                        f.attributes.insert(
                            "valueOfSounding".into(),
                            ferrite_lua::AttributeValue::Real(updated),
                        );
                        break;
                    }
                }
            }
        }
        ensure!(
            evidence.is_some(),
            "No real numeric hazard attribute for changed-input control"
        );
        evidence
    } else {
        None
    };
    let data = data.read().unwrap();
    let before = format!("{:?}", data.features); // same owned map instance; no reconstruction/reordering
    let mut outputs = Vec::new();
    let mut measures = Vec::new();
    for shared in [false, true] {
        let before_stats = if shared {
            ChunkCache::process_shared().stats()
        } else {
            Default::default()
        };
        let started = Instant::now();
        let mut e = engine(Arc::clone(&sources), fc, shared)?;
        let full = e.process_cell(&data, ctx.clone())?;
        let mut ids: Vec<_> = data.features.keys().copied().collect();
        ids.sort_unstable();
        let selected: Vec<_> = ids.into_iter().step_by(7).collect();
        ensure!(!selected.is_empty(), "No selected features");
        let partial = e.session_mut().execute_portrayal_for(selected.clone())?;
        for r in &partial {
            ensure!(
                selected.contains(&r.feature_id.parse::<i64>()?),
                "Unexpected selected result ID"
            );
        }
        // Functional parity includes the complete host input and selected rerun.
        // This clone prepares the test fixture, not the production owned path.
        let mut owned_engine = engine(Arc::clone(&sources), fc, shared)?;
        let owned_full = owned_engine.process_owned_cell(data.clone(), ctx.clone())?;
        let owned_partial = owned_engine.session_mut().execute_portrayal_for(selected.clone())?;
        ensure!(canonical(&full) == canonical(&owned_full), "Owned full typed mismatch in {label}");
        ensure!(canonical(&partial) == canonical(&owned_partial), "Owned selected typed mismatch in {label}");
        let full = canonical(&full);
        let partial = canonical(&partial);
        let stats = e.session().chunk_cache_stats();
        ensure!(
            stats.bytecode_hits > before_stats.bytecode_hits,
            "No compiler reuse observed"
        );
        ensure!(
            stats.entries <= 1024 && stats.retained_payload_bytes <= 16 * 1024 * 1024,
            "Compiler retention budget exceeded"
        );
        measures.push(serde_json::json!({"shared":shared,"elapsed_ms":started.elapsed().as_secs_f64()*1000.,"stats":format!("{stats:?}"),"compiled_delta":stats.compiled_chunks-before_stats.compiled_chunks,"hit_delta":stats.bytecode_hits-before_stats.bytecode_hits,"full_sha256":hex(&Sha256::digest(&full)),"selected_sha256":hex(&Sha256::digest(&partial))}));
        outputs.push((full, partial));
    }
    for (index, (full, selected)) in outputs.iter().enumerate() {
        let mode = if index == 0 {
            "engine-local"
        } else {
            "process-shared"
        };
        fs::write(out.join(format!("{label}-{mode}-full.txt")), full)?;
        fs::write(out.join(format!("{label}-{mode}-selected.txt")), selected)?;
    }
    if outputs[0] != outputs[1] {
        let mut differences = Vec::new();
        for (scope, left, right) in [
            ("full", &outputs[0].0, &outputs[1].0),
            ("selected", &outputs[0].1, &outputs[1].1),
        ] {
            if left == right {
                continue;
            }
            let at = left
                .iter()
                .zip(right.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(left.len().min(right.len()));
            let start = at.saturating_sub(500);
            differences.push(serde_json::json!({"scope":scope,"first_different_byte":at,"baseline_bytes":left.len(),"shared_bytes":right.len(),
                "baseline_context":String::from_utf8_lossy(&left[start.min(left.len())..(at+1500).min(left.len())]),
                "shared_context":String::from_utf8_lossy(&right[start.min(right.len())..(at+1500).min(right.len())])}));
        }
        let diagnostic = serde_json::json!({"label":label,"differences":differences,"measures":measures,"equality_gate_unchanged":true});
        fs::write(
            out.join(format!("{label}-mismatch.json")),
            serde_json::to_vec_pretty(&diagnostic)?,
        )?;
        eprintln!("{}", serde_json::to_string_pretty(&diagnostic)?);
    }
    ensure!(
        outputs[0] == outputs[1],
        "Full typed differential mismatch in {label}"
    );
    ensure!(
        before == format!("{:?}", data.features),
        "Host mutated input attributes"
    );
    fs::write(out.join(format!("{label}-full.txt")), &outputs[0].0)?;
    fs::write(out.join(format!("{label}-selected.txt")), &outputs[0].1)?;
    Ok(
        serde_json::json!({"label":label,"fc_digest":hex(fc.source_digest()),"pc_digest":hex(sources.digest()),"context":ctx.to_lua_params(),"features":data.features.len(),"derived_feature_input_change":feature_input_change,"comparison":"complete ordered typed bytes, not hashes","modes":measures,"context_diagnostics":ferrite_s101::validate_portrayal_context(pc,&ctx)?.1}),
    )
}
fn owned_abba(
    fc: &BoundFeatureCatalogue,
    pc: &BoundPortrayalCatalogue,
    cell: &S101Cell,
    out: &Path,
) -> Result<()> {
    let sources = pc.sources();
    let mut ctx = ContextParameters::from_pc_context(pc.get_context_parameters());
    ferrite_s101::synchronize_legacy_context(pc, &mut ctx);
    ferrite_s101::validate_portrayal_context(pc, &ctx)?;
    let before = format!("{cell:?}");
    let mut warm = engine(Arc::clone(&sources), fc, true)?;
    let warm_context = PortrayalContext::from_cell(cell, ctx.clone());
    let warm_data = warm_context.cell_data();
    let expected = canonical(&warm.process_cell(&warm_data.read().unwrap(), ctx.clone())?);
    drop(warm_data); drop(warm_context); drop(warm);
    let mut rows = Vec::new();
    for (index, owned) in [false,true,true,false,true,false,false,true].into_iter().enumerate() {
        let stats_before = ChunkCache::process_shared().stats();
        let start = Instant::now();
        // Includes conversion from original S101Cell, engine/FC metadata preparation,
        // context acquisition, all host-data preparation/cloning and full portrayal.
        let context = PortrayalContext::from_cell(cell, ctx.clone());
        let mut current = engine(Arc::clone(&sources), fc, true)?;
        let results = if owned {
            current.process_owned_cell(context.into_cell_data()?, ctx.clone())?
        } else {
            let data = context.cell_data();
            let result = current.process_cell(&data.read().unwrap(), ctx.clone())?;
            drop(data); drop(context);
            result
        };
        let elapsed_ms = start.elapsed().as_secs_f64()*1000.;
        let stats = ChunkCache::process_shared().stats();
        ensure!(stats.compiled_chunks == stats_before.compiled_chunks, "Unexpected compilation in ABBA");
        let bytes = canonical(&results); // comparison/file IO excluded from timer
        ensure!(bytes == expected, "Owned ABBA full ordered typed mismatch {index}");
        ensure!(format!("{cell:?}") == before, "ABBA mutated original attributes");
        rows.push(serde_json::json!({"index":index,"owned":owned,"elapsed_ms":elapsed_ms,
            "compiled_delta":stats.compiled_chunks-stats_before.compiled_chunks,
            "hit_delta":stats.bytecode_hits-stats_before.bytecode_hits,
            "full_bytes":bytes.len(),"full_sha256":hex(&Sha256::digest(&bytes))}));
    }
    fs::write(out.join("owned-cell-abba.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "scope":"samebinary ABBA+BAAB, fresh VM; includes S101Cell->CellData conversion, FC metadata/engine preparation, context and borrowed clone or owned handoff, full Lua portrayal. Excludes first compiler warmup, canonical formatting/equality, IO and output conversion. Not native/FPS.",
        "order":"borrowed owned owned borrowed owned borrowed borrowed owned", "rows":rows}))?)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.len() >= 4,
        "Pass FC, PC root, new output dir, base and ordered updates"
    );
    let fc_path = PathBuf::from(&args[0]);
    let pc_path = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    ensure!(
        !out.exists(),
        "Output must be new (preserve earlier attempts)"
    );
    fs::create_dir_all(&out)?;
    let fc = FeatureCatalogue::load_bound(&fc_path)?;
    let pc = PortrayalCatalogue::load_bound(&pc_path)?;
    ferrite_s101::validate_catalogue_pair(&fc, &pc)?;
    let fc_before = file_digest(&fc_path)?;
    ensure!(
        hex(fc.source_digest()) == fc_before.0,
        "FC changed between parse and guard"
    );
    let pc_digest = *pc.sources().digest();
    let originals: Vec<PathBuf> = args[3..].iter().map(PathBuf::from).collect();
    let mut snapshots = Vec::new();
    let mut chain = Vec::new();
    let mut total = 0u64;
    for path in &originals {
        let snap = UnauthenticatedSnapshot::copy_bounded(path, 512 * 1024 * 1024 - total)?;
        let digest = file_digest(snap.path())?;
        ensure!(
            digest == file_digest(path)?,
            "Original changed during capture"
        );
        total += digest.1;
        chain.push(serde_json::json!({"original":path,"sha256":digest.0,"bytes":digest.1}));
        snapshots.push(snap);
    }
    let updates: Vec<_> = snapshots[1..]
        .iter()
        .map(|s| s.path().to_path_buf())
        .collect();
    let (mut cell, identity) = S101Cell::load_update_chain_from_with_identity(
        &originals[0],
        snapshots[0].path(),
        &updates,
    )?;
    ferrite_s101::validate_dataset_catalogues(&cell.dsid, &fc, &pc.product_id, &pc.version)?;
    cell.normalize_feature_codes(&fc.feature_type_codes());
    let mut source_hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&identity, &mut source_hasher);
    let source_key = std::hash::Hasher::finish(&source_hasher);
    let expected_audit = if let Ok(path) = std::env::var("FERRITE_EXPECTED_AUDIT") {
        let audit: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        let expected = &audit["original"]["cells"][0];
        ensure!(
            audit["original"]["chain_paths"][0] == serde_json::to_value(&originals)?,
            "Different original chain paths"
        );
        ensure!(
            expected["source_key"] == source_key,
            "Raw ordered chain identity mismatch"
        );
        ensure!(
            expected["features"] == cell.features.len(),
            "Materialized feature count mismatch"
        );
        ensure!(
            expected["edition"] == cell.dsid.edition_number
                && expected["update"] == cell.dsid.update_number,
            "Edition/update mismatch"
        );
        Some(
            serde_json::json!({"path":path,"source_key":source_key,"complete_cell_debug_sha256_note":"Debug of randomized source HashMaps is not a stable cross-process identity. Raw ordered identity, feature count, edition/update are gated; source attributes are preserved and emitted independently."}),
        )
    } else {
        None
    };

    ensure!(!cell.features.is_empty(), "No materialized features");
    let source_before = format!("{cell:?}");
    fs::write(out.join("materialized-source-debug.txt"), &source_before)?;
    let default = ContextParameters::from_pc_context(pc.get_context_parameters());
    let mut rows = Vec::new();
    for (label, depth, shallow, palette) in [
        (
            "default",
            default.safety_contour,
            default.shallow_water_dangers,
            "Day",
        ),
        ("shallow-off", 5., false, "Dusk"),
        ("deep-on", 50., true, "Night"),
        (
            "default-repeat",
            default.safety_contour,
            default.shallow_water_dangers,
            "Day",
        ),
    ] {
        let mut ctx = default.clone();
        ctx.safety_depth = depth;
        ctx.safety_contour = depth;
        ctx.shallow_water_dangers = shallow;
        ctx.palette = palette.into();
        ferrite_s101::synchronize_legacy_context(&pc, &mut ctx);
        ferrite_s101::validate_portrayal_context(&pc, &ctx)?;
        rows.push(compare(&fc, &pc, &cell, pc.sources(), ctx, label, &out)?);
    }
    let mut changed_ctx = ContextParameters::from_pc_context(pc.get_context_parameters());
    ferrite_s101::synchronize_legacy_context(&pc, &mut changed_ctx);
    rows.push(compare(
        &fc,
        &pc,
        &cell,
        pc.sources(),
        changed_ctx,
        "feature-input-change",
        &out,
    )?);
    ensure!(fs::read(out.join("default-full.txt"))? != fs::read(out.join("feature-input-change-full.txt"))?,
        "Changed hazard input did not produce a distinct official-PC output; fixture is inconclusive");
    // Optional second real FC input; not a fabricated or silently edited catalogue.
    let alternate_fc_evidence = if let Ok(path) = std::env::var("FERRITE_TEST_ALTERNATE_FC") {
        let alternate = FeatureCatalogue::load_bound(&path)?;
        ferrite_s101::validate_catalogue_pair(&alternate, &pc)?;
        ferrite_s101::validate_dataset_catalogues(
            &cell.dsid,
            &alternate,
            &pc.product_id,
            &pc.version,
        )?;
        ensure!(
            alternate.source_digest() != fc.source_digest(),
            "Alternate FC must actually differ"
        );
        let mut ctx = ContextParameters::from_pc_context(pc.get_context_parameters());
        ferrite_s101::synchronize_legacy_context(&pc, &mut ctx);
        rows.push(compare(
            &alternate,
            &pc,
            &cell,
            pc.sources(),
            ctx,
            "alternate-fc",
            &out,
        )?);
        ensure!(
            file_digest(Path::new(&path))?.0 == hex(alternate.source_digest()),
            "Alternate FC changed during differential"
        );
        Some(serde_json::json!({"path":path,"digest":hex(alternate.source_digest())}))
    } else {
        None
    };
    owned_abba(&fc, &pc, &cell, &out)?;
    // Copy only the official retained Rules bytes to an owned miniature PC map.
    // A/B use the SAME chunk path; original files are never modified.
    let private = out.join("owned-pc");
    fs::create_dir_all(private.join("Rules"))?;
    for item in walkdir::WalkDir::new(pc_path.join("Rules")) {
        let item = item?;
        ensure!(!item.file_type().is_symlink(), "Rules symlink rejected");
        if item.file_type().is_file() {
            let rel = item.path().strip_prefix(&pc_path)?;
            let bytes = pc.sources().read_relative(rel)?;
            let target = private.join(rel);
            fs::create_dir_all(target.parent().unwrap())?;
            fs::write(target, &bytes)?;
        }
    }
    let a = CatalogueSources::capture(&private)?;
    let mut warm = engine(Arc::clone(&a), &fc, true)?;
    let mut ctx = default;
    ferrite_s101::synchronize_legacy_context(&pc, &mut ctx);
    let portrayal = PortrayalContext::from_cell(&cell, ctx.clone());
    let data = portrayal.cell_data();
    let guard = data.read().unwrap();
    let before = canonical(&warm.process_cell(&guard, ctx.clone())?);
    fs::write(private.join("Rules/main.lua"), b"this is not valid Lua !!!")?;
    let b = CatalogueSources::capture(&private)?;
    ensure!(
        a.digest() != b.digest(),
        "Source edit failed to change PC digest"
    );
    for shared in [false, true] {
        ensure!(
            engine(Arc::clone(&b), &fc, shared).is_err(),
            "Invalid PC source served stale bytecode"
        );
    }
    let mut retained = engine(Arc::clone(&a), &fc, true)?;
    ensure!(
        before == canonical(&retained.process_cell(&guard, ctx)?),
        "Retained A snapshot changed after same-path B syntax failure"
    );
    ensure!(
        source_before == format!("{cell:?}"),
        "Source cell/attributes changed"
    );
    ensure!(fc_before == file_digest(&fc_path)?, "FC changed during run");
    ensure!(
        pc_digest == *CatalogueSources::capture(&pc_path)?.digest(),
        "PC changed during run"
    );
    for (path, entry) in originals.iter().zip(&chain) {
        let after = file_digest(path)?;
        ensure!(
            entry["sha256"] == after.0 && entry["bytes"] == after.1,
            "Original chain changed"
        );
    }
    fs::write(
        out.join("report.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"scope":"CPU Lua compilation/portrayal equivalence only; no renderer palette conversion, native frames or FPS","authentication":"local CPU fixture retained unauthenticated snapshots; no signature claim","source_identity_sha256":hex(identity.sha256()),"source_key":source_key,"expected_audit":expected_audit,"dataset":format!("{:?}",cell.dsid),"complete_chain":chain,"alternate_fc":alternate_fc_evidence,"materialized_features":cell.features.len(),"rows":rows,"owned_borrowed_full_selected_differential":true,"owned_abba_file":"owned-cell-abba.json","same_path_changed_source_rejected":true,"retained_a_survives_b":true,"all_original_inputs_unchanged":true,"runtime_identity":format!("{:?}",ferrite_lua::runtime_identity())}),
        )?,
    )?;
    Ok(())
}
