use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf, process::Command};

fn field(hash: &mut Sha256, name: &str, bytes: &[u8]) {
    hash.update((name.len() as u64).to_le_bytes());
    hash.update(name.as_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Lua for wasm32-unknown-unknown: mlua-sys cannot vendor it there, so the
/// same lua-src sources are compiled here with clang's wasm32 target and
/// linked with the minimal C library in `wasm-libc/`. lua-src is driven with
/// its WASI configuration (`wasm32-wasip1`: LUA_USE_POSIX with the WASI
/// guards, LUA_COMPAT_5_3, setjmp/longjmp as wasm exception handling); its
/// objects are plain wasm32 objects, and our headers stand in for a sysroot.
fn build_wasm_lua(manifest: &std::path::Path) {
    let version = if env::var_os("CARGO_FEATURE_LUA55").is_some() {
        lua_src::Version::Lua55
    } else {
        lua_src::Version::Lua54
    };
    for name in ["FERRITE_WASM_CC", "FERRITE_WASM_AR"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!(
        "cargo:rerun-if-changed={}",
        manifest.join("wasm-libc").display()
    );
    // A clang with the WebAssembly backend (Homebrew/upstream LLVM or Apple
    // clang) and an archiver that writes wasm symbol tables (llvm-ar).
    let llvm_dirs = [
        "/opt/homebrew/opt/llvm/bin",
        "/usr/local/opt/llvm/bin",
        "/usr/lib/llvm/bin",
    ];
    let tool = |var: &str, name: &str| -> PathBuf {
        env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
            llvm_dirs
                .iter()
                .map(|d| std::path::Path::new(d).join(name))
                .find(|p| p.is_file())
                .unwrap_or_else(|| PathBuf::from(name))
        })
    };
    let clang = tool("FERRITE_WASM_CC", "clang");
    let ar = tool("FERRITE_WASM_AR", "llvm-ar");
    let include = manifest.join("wasm-libc/include");
    // lua-src builds with its own cc::Build; point the WASI target at our tools.
    env::set_var("CC_wasm32_wasip1", &clang);
    env::set_var("AR_wasm32_wasip1", &ar);
    env::set_var(
        "CFLAGS_wasm32_wasip1",
        format!("-isystem {}", include.display()),
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let artifacts = lua_src::Build::new()
        .target("wasm32-wasip1")
        .out_dir(out.join("lua-wasm"))
        .try_build(version)
        .unwrap_or_else(|e| {
            panic!(
                "building Lua for wasm32 with {} failed ({e}); set FERRITE_WASM_CC to a clang \
                 with the WebAssembly target and FERRITE_WASM_AR to llvm-ar",
                clang.display()
            )
        });
    // The C library is compiled without the setjmp lowering (it implements it).
    cc::Build::new()
        .compiler(&clang)
        .archiver(&ar)
        .target("wasm32-unknown-unknown")
        .cargo_metadata(false)
        .include(&include)
        .flag("-mexception-handling")
        .flag("-ffreestanding")
        .file(manifest.join("wasm-libc/ferrite_libc.c"))
        .out_dir(out.join("wasm-libc"))
        .compile("ferrite_wasm_libc");
    println!(
        "cargo:rustc-link-search=native={}",
        artifacts.lib_dir().display()
    );
    for lib in artifacts.libs().iter().filter(|l| l.starts_with("lua")) {
        println!("cargo:rustc-link-lib=static={lib}");
    }
    println!(
        "cargo:rustc-link-search=native={}",
        out.join("wasm-libc").display()
    );
    println!("cargo:rustc-link-lib=static=ferrite_wasm_libc");
}

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32")
        && env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("unknown")
    {
        build_wasm_lua(&manifest);
    }
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
    for name in fs::read_dir(manifest.join("wasm-libc/include"))
        .into_iter()
        .flatten()
        .chain(
            fs::read_dir(manifest.join("wasm-libc/include/sys"))
                .into_iter()
                .flatten(),
        )
        .chain(
            fs::read_dir(manifest.join("wasm-libc"))
                .into_iter()
                .flatten(),
        )
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "c" || e == "h"))
    {
        sources.push(name);
    }
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
