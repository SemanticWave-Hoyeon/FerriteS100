`python3 tools/build.py` now obtains Cargo's effective `target_directory` with
`cargo metadata --no-deps --offline --locked`, then explicitly passes that path
to Cargo. Global/ancestor configuration and CARGO_TARGET_DIR remain effective.
Metadata does not compile; its invocation errors stop the build.

A kernel advisory lock on that target spans inventory, invalidation, Cargo,
post-build verification and atomic success-state publication. POSIX uses flock;
Windows uses msvcrt byte-range locking. Process death releases the lock. All
concurrent writers must use this guard. Direct cargo, IDEs, or other scripts can
bypass it and invalidate the receipt. Lock reliability on network filesystems
must be qualified separately; this is intended for local build filesystems.

V2 manifest covers Rust, WGSL and common native source extensions; Cargo manifests
and lock; .cargo config files; assets and Catalogues; and literal include!/include_str!/
include_bytes! dependencies regardless extension. Build-script src trees and
literal rerun-if-changed directory/file inputs are conservative inputs. README,
general docs, .DS_Store and runtime log/cache files do not enter the manifest
unless explicitly embedded (excluded logs are rejected if embedded). Runtime
folders named target/work/outputs, VCS and Python cache folders are excluded.
Those excluded folders must not supply compiled input. Symlink sources are
refused. Nonliteral include expressions and dynamic external build-script inputs
must be separately admitted/qualified; this is not an arbitrary build.rs proof.

Unknown/corrupt state or changed owner path invalidates all admitted source
mtimes. Existing input changes touch only the changed files: Cargo dep-info and
build-script rerun declarations handle their consumers. Added/deleted inputs
invalidate only the nearest Cargo package's lib/main/build.rs/Cargo manifest and
bin/example entrypoints, because stale dep-info may not mention them. Root global
structural inputs also invalidate package build-script anchors. Regular main.rs
edits therefore do not touch unrelated local crate anchors. All-source owner
invalidation remains necessary for CARGO_MANIFEST_DIR/env! differences.
Prior Cargo fingerprint invocation times participate in the strictly monotonic
timestamp gate. Coarse filesystem or future timestamps reject rather than
register success. No artifact deletion, fabricated source edit, compiler/profile/
feature fingerprint substitution or cargo clean occurs.

Only Cargo exit0 with identical before/after content and post-invalidation
mtime/size stamps publishes state via atomic replace. Old state is removed before
Cargo, because failed/interrupted builds may partially replace artifacts. Usual
edit-and-restore during compilation is rejected too. An adversary restoring both
bytes and all metadata between observations is outside this filesystem polling
contract. The receipt is not a cryptographic attestation of compiler output.

The state is target-wide; Cargo continues to distinguish toolchains, profiles,
features, flags, target triples and environment. Source invalidation makes their
old invocation timestamps stale. No direct Cargo/native test has been executed
for this candidate; Python unit tests mock compilation and test guard behavior.
