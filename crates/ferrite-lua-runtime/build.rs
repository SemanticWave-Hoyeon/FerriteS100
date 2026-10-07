use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf, process::Command};

fn field(hash: &mut Sha256, name: &str, bytes: &[u8]) {
    hash.update((name.len() as u64).to_le_bytes());
    hash.update(name.as_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let lock = env::var_os("FERRITE_LUA_LOCK_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            manifest
                .ancestors()
                .map(|p| p.join("Cargo.lock"))
                .find(|p| p.is_file())
        });
    println!("cargo:rerun-if-env-changed=FERRITE_LUA_LOCK_PATH");
    let mut complete = true;
    let mut hash = Sha256::new();
    field(&mut hash, "identity-format", b"ferrite-lua-build-v2");
    match lock {
        Some(lock) => {
            if lock.is_file() {
                println!("cargo:rerun-if-changed={}", lock.display());
            }
            match fs::read(&lock) {
                Ok(bytes) => field(&mut hash, "dependency-lock", &bytes),
                Err(_) => complete = false,
            }
            // Workspace profiles/features can change the vendored mlua-sys C
            // build while this crate's own OPT_LEVEL remains unchanged.
            if let Some(root) = lock.parent() {
                let workspace_manifest = root.join("Cargo.toml");
                if workspace_manifest.is_file() {
                    println!("cargo:rerun-if-changed={}", workspace_manifest.display());
                    field(
                        &mut hash,
                        "Cargo.toml",
                        &fs::read(workspace_manifest).expect("read workspace build configuration"),
                    );
                } else {
                    complete = false;
                }
                // Never watch an absent file: Cargo treats it as dirty on every
                // invocation. Watch the existing, dedicated configuration directory
                // so adding/removing either config filename also invalidates us.
                // Watching the workspace root would recursively include target output.
                let config_directory = root.join(".cargo");
                let safe_directory_watch = config_directory
                    .canonicalize()
                    .ok()
                    .zip(
                        PathBuf::from(env::var_os("OUT_DIR").unwrap())
                            .canonicalize()
                            .ok(),
                    )
                    .is_some_and(|(config, output)| config.is_dir() && !output.starts_with(config));
                if safe_directory_watch {
                    println!("cargo:rerun-if-changed={}", config_directory.display());
                } else {
                    // No bounded creation watch is available when .cargo is absent,
                    // or when Cargo generates output within it. Keep the VM usable;
                    // disable only persistent output caching for this configuration.
                    complete = false;
                    println!("cargo:warning=Lua persistent cache identity unavailable: .cargo must exist outside generated build output");
                }
                for name in [".cargo/config.toml", ".cargo/config"] {
                    let path = root.join(name);
                    if path.is_file() {
                        // File watches remain safe even if directory watching is not.
                        println!("cargo:rerun-if-changed={}", path.display());
                        field(&mut hash, &format!("{name}-state"), b"present");
                        field(
                            &mut hash,
                            name,
                            &fs::read(path).expect("read Cargo build configuration"),
                        );
                    } else {
                        field(&mut hash, &format!("{name}-state"), b"absent");
                    }
                }
            }
        }
        None => complete = false,
    }
    // A separately packaged dependency may not see the application's Cargo.lock.
    // It remains usable in that case; only persistent output caching is disabled.
    println!("cargo:rerun-if-changed=src");
    let mut sources = vec![manifest.join("build.rs"), manifest.join("Cargo.toml")];
    fn collect(path: &std::path::Path, sources: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).expect("read runtime source directory") {
            let path = entry.expect("runtime source entry").path();
            if path.is_dir() {
                collect(&path, sources);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push(path);
            }
        }
    }
    collect(&manifest.join("src"), &mut sources);
    sources.sort();
    for path in sources {
        let name = path.strip_prefix(&manifest).unwrap().to_string_lossy();
        println!("cargo:rerun-if-changed={}", path.display());
        field(
            &mut hash,
            &name,
            &fs::read(&path).expect("read runtime source"),
        );
    }
    // These are build inputs, not a claim to identify an already linked object.
    let mut inputs: Vec<_> = env::vars()
        .filter(|(name, _)| {
            name.starts_with("CARGO_CFG_")
                || name.starts_with("CARGO_FEATURE_")
                || name.starts_with("CC")
                || name.starts_with("CFLAGS")
                || name.starts_with("CPPFLAGS")
                || name.starts_with("AR_")
                || matches!(
                    name.as_str(),
                    "TARGET"
                        | "HOST"
                        | "PROFILE"
                        | "OPT_LEVEL"
                        | "DEBUG"
                        | "CARGO_ENCODED_RUSTFLAGS"
                        | "RUSTFLAGS"
                        | "RUSTC"
                        | "RUSTC_WRAPPER"
                        | "RUSTC_WORKSPACE_WRAPPER"
                        | "AR"
                        | "SDKROOT"
                        | "MACOSX_DEPLOYMENT_TARGET"
                )
        })
        .collect();
    inputs.sort();
    for (name, value) in inputs {
        println!("cargo:rerun-if-env-changed={name}");
        field(&mut hash, &name, value.as_bytes());
    }
    // Also watch absent flags so introducing one invalidates a prior fingerprint.
    for name in [
        "CC",
        "CFLAGS",
        "CPPFLAGS",
        "AR",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let rustc = Command::new(env::var_os("RUSTC").expect("RUSTC selected by Cargo"))
        .arg("-vV")
        .output()
        .expect("identify selected rustc");
    complete &= rustc.status.success();
    field(&mut hash, "rustc-version", &rustc.stdout);
    // lua-src uses cc::Build to select the C compiler under the same Cargo target
    // and CC/CFLAGS environment. This probes that tool selection, not an unrelated
    // PATH cc or the exact vendor compile command (package profile/defines differ).
    // Workspace profiles and the pinned lua-src build code are separate inputs.
    let compiler = cc::Build::new().get_compiler();
    field(
        &mut hash,
        "c-tool-selection-probe",
        format!("{:?}", compiler.to_command()).as_bytes(),
    );
    let mut version = compiler.to_command();
    version.arg(if compiler.is_like_msvc() {
        "/?"
    } else {
        "--version"
    });
    let output = version.output().expect("identify selected C compiler");
    complete &= (output.status.success() || compiler.is_like_msvc())
        && (!output.stdout.is_empty() || !output.stderr.is_empty());
    field(&mut hash, "c-compiler-stdout", &output.stdout);
    field(&mut hash, "c-compiler-stderr", &output.stderr);
    let fingerprint = format!("{:x}", hash.finalize());
    let generated = format!("pub const BUILD_FINGERPRINT: &str = {fingerprint:?};\npub const BUILD_ID_COMPLETE: bool = {complete};\n");
    let output_path =
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("runtime_build_identity.rs");
    // Preserve generated-source mtime on an otherwise equivalent build-script run.
    if fs::read(&output_path).ok().as_deref() != Some(generated.as_bytes()) {
        fs::write(output_path, generated).expect("save runtime build identity");
    }
}
