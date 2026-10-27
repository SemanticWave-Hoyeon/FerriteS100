//! Isolated S-102 2.1 trial inspection. The HDF reader runs in a child process
//! with a time and output budget; only the validated `TrialInspection` wire
//! (metadata, no samples) returns. Trial grids never become depth coverage:
//! a missing vertical datum is reported, never inferred.
use crate::trial_inspection::{
    TrialInspection, TrialInspectionRequest, TrialSourceIdentity, MAX_WIRE_BYTES,
};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub const FLAG: &str = "--internal-s102-trial-worker";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SOURCE_BYTES: u64 = 512 * 1024 * 1024;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(text: &str) -> Result<[u8; 32]> {
    ensure!(text.len() == 64, "trial worker digest length");
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16)?;
    }
    Ok(out)
}

/// Length and SHA-256 of the file as the parent read it.
fn identity(path: &Path) -> Result<TrialSourceIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(metadata.is_file(), "S-102 trial input must be a real file");
    ensure!(
        metadata.len() > 0 && metadata.len() <= MAX_SOURCE_BYTES,
        "S-102 trial input size outside the 512MiB receiver limit"
    );
    let mut file = File::open(path)?.take(MAX_SOURCE_BYTES + 1);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut byte_length = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        byte_length += n as u64;
        hasher.update(&buffer[..n]);
    }
    ensure!(
        byte_length == metadata.len(),
        "S-102 trial input changed while reading"
    );
    Ok(TrialSourceIdentity {
        byte_length,
        sha256: hasher.finalize().into(),
    })
}

pub fn run_child() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    ensure!(
        args.next().as_deref() == Some(std::ffi::OsStr::new(FLAG)),
        "worker flag missing"
    );
    let source = args.next().context("worker source missing")?;
    let output = args.next().context("worker output missing")?;
    let length: u64 = args
        .next()
        .and_then(|s| s.into_string().ok())
        .context("worker length missing")?
        .parse()?;
    let digest = unhex(
        &args
            .next()
            .and_then(|s| s.into_string().ok())
            .context("worker digest missing")?,
    )?;
    ensure!(args.next().is_none(), "extra worker argument");
    let request = TrialInspectionRequest {
        source: TrialSourceIdentity {
            byte_length: length,
            sha256: digest,
        },
        window: None,
    };
    let grid = crate::legacy_trial::LegacyTrialGrid::open(Path::new(&source))?;
    let inspection = TrialInspection::from_grid(&grid, request)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(Path::new(&output))?;
    let mut writer = BufWriter::new(file);
    inspection.encode(&mut writer)?;
    writer.flush()?;
    Ok(())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Metadata-only inspection of one S-102 2.1 trial file.
pub fn inspect(path: &Path) -> Result<TrialInspection> {
    let source = identity(path)?;
    let request = TrialInspectionRequest {
        source,
        window: None,
    };
    let folder = tempfile::tempdir().context("create private S-102 trial worker directory")?;
    let output = folder.path().join("trial.bin");
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(FLAG)
        .arg(path)
        .arg(&output)
        .arg(source.byte_length.to_string())
        .arg(hex(&source.sha256))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = ChildGuard(
        command
            .spawn()
            .context("start isolated S-102 trial inspector")?,
    );
    let start = Instant::now();
    let status = loop {
        if let Ok(metadata) = fs::metadata(&output) {
            ensure!(
                metadata.len() <= MAX_WIRE_BYTES,
                "trial worker output exceeds limit"
            );
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        ensure!(start.elapsed() < TIMEOUT, "S-102 trial inspector timed out");
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure!(
        status.success(),
        "S-102 trial inspector rejected input or exited abnormally"
    );
    let file = File::open(&output).context("trial worker reply missing")?;
    ensure!(
        file.metadata()?.len() <= MAX_WIRE_BYTES,
        "trial worker output exceeds limit"
    );
    TrialInspection::decode(file, request)
}

/// Metadata-only inspection of S-102 2.1 trial bytes held by a data source,
/// decoded in this process with the pure-Rust HDF5 reader (no child-process
/// isolation; hosts without processes, such as the web, use this path).
pub fn inspect_bytes(bytes: std::sync::Arc<[u8]>) -> Result<TrialInspection> {
    let length = bytes.len() as u64;
    ensure!(
        length > 0 && length <= MAX_SOURCE_BYTES,
        "S-102 trial input size outside the 512MiB receiver limit"
    );
    let request = TrialInspectionRequest {
        source: TrialSourceIdentity {
            byte_length: length,
            sha256: Sha256::digest(&bytes).into(),
        },
        window: None,
    };
    let grid = crate::legacy_trial::LegacyTrialGrid::open_bytes(bytes)?;
    TrialInspection::from_grid(&grid, request)
}
/// One-line description for the data tree.
pub fn summary(inspection: &TrialInspection) -> String {
    use crate::legacy_trial::TrialVerticalDatum;
    let m = inspection.metadata();
    let datum = match m.vertical_datum {
        TrialVerticalDatum::Missing => "vertical datum missing".to_owned(),
        TrialVerticalDatum::Encoded(code) => format!("vertical datum code {code}"),
    };
    format!(
        "{} · {} · {}×{} nodes · {datum} · inspection only, not used for depth",
        m.product_specification, m.issue_date_lexical, m.width, m.height
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn digest_round_trips_and_rejects_malformed_text() {
        let digest: [u8; 32] = std::array::from_fn(|i| (i * 7) as u8);
        assert_eq!(unhex(&hex(&digest)).unwrap(), digest);
        assert!(unhex("00").is_err());
        assert!(unhex(&"zz".repeat(32)).is_err());
    }
}
