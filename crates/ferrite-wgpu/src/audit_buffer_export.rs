//! Explicit offline evidence export. Hash mode avoids repeated multi-MiB dumps;
//! it hashes every supplied byte and never changes rendering or timed callbacks.
use sha2::{Digest, Sha256};
use std::{ffi::OsStr, io, path::Path};

pub fn digest_only_enabled() -> bool {
    enabled(std::env::var_os("FERRITE_AUDIT_DIGEST_ONLY").as_deref())
}
fn enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

pub(crate) fn write_buffer(
    directory: &Path,
    name: &str,
    bytes: &[u8],
    digest_only: bool,
) -> io::Result<()> {
    if digest_only {
        let evidence = serde_json::json!({
            "byte_len": bytes.len(),
            "sha256": format!("{:x}", Sha256::digest(bytes)),
            "raw_buffer_retained": false,
            "scope": "Digest of the complete supplied byte stream, including order; not a signature, authority or device readback claim"
        });
        std::fs::write(
            directory.join(format!("{name}.sha256.json")),
            serde_json::to_vec_pretty(&evidence)?,
        )
    } else {
        std::fs::write(directory.join(format!("{name}.bin")), bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_opt_in_and_complete_byte_stream_evidence() {
        for value in [
            None,
            Some(OsStr::new("0")),
            Some(OsStr::new("true")),
            Some(OsStr::new("1 ")),
        ] {
            assert!(!enabled(value));
        }
        assert!(enabled(Some(OsStr::new("1"))));
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ferrite-audit-export-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        write_buffer(&dir, "payload", b"abc", true).unwrap();
        assert!(!dir.join("payload.bin").exists());
        let digest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("payload.sha256.json")).unwrap())
                .unwrap();
        assert_eq!(digest["byte_len"], 3);
        assert_eq!(
            digest["sha256"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        write_buffer(&dir, "reordered", b"acb", true).unwrap();
        let other: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("reordered.sha256.json")).unwrap())
                .unwrap();
        assert_ne!(digest["sha256"], other["sha256"]);
        write_buffer(&dir, "legacy", &[0, 255, 1, 0], false).unwrap();
        assert_eq!(
            std::fs::read(dir.join("legacy.bin")).unwrap(),
            [0, 255, 1, 0]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
