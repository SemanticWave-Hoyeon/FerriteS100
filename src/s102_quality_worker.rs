//! Private S-102 quality-table decoder; bounded NDJSON in a private file.
use anyhow::{ensure, Context, Result};
use ferrite_s102::{BathymetryCoverage, QualityRecord};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub const FLAG: &str = "--internal-s102-quality-worker";
const MAX_ROWS: usize = 100_000;
const MAX_WIRE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_HEADER_BYTES: usize = 256;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u32,
    has_quality: bool,
    row_count: usize,
}

struct LimitWriter<W> {
    inner: W,
    used: usize,
    limit: usize,
}
impl<W: Write> Write for LimitWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.used) {
            return Err(std::io::Error::other(
                "quality worker output budget exceeded",
            ));
        }
        let n = self.inner.write(bytes)?;
        self.used += n;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

pub fn run_child() -> Result<()> {
    let mut args = std::env::args_os();
    let _program = args.next();
    ensure!(
        args.next().as_deref() == Some(std::ffi::OsStr::new(FLAG)),
        "worker flag missing"
    );
    let source = args.next().context("worker source missing")?;
    let output = args.next().context("worker output missing")?;
    ensure!(args.next().is_none(), "extra worker argument");
    let coverages = BathymetryCoverage::open(Path::new(&source))?;
    let quality = coverages.first().and_then(|c| c.quality.as_ref());
    let header = Header {
        version: 2,
        has_quality: quality.is_some(),
        row_count: quality.map_or(0, |q| q.record_count()),
    };
    ensure!(header.row_count <= MAX_ROWS, "too many quality records");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(Path::new(&output))?;
    let mut writer = LimitWriter {
        inner: BufWriter::new(file),
        used: 0,
        limit: MAX_WIRE_BYTES,
    };
    serde_json::to_writer(&mut writer, &header)?;
    writer.write_all(b"\n")?;
    if let Some(quality) = quality {
        for row in quality.records() {
            row.validate_worker_record()?;
            let before = writer.used;
            serde_json::to_writer(&mut writer, row)?;
            ensure!(
                writer.used - before <= MAX_RECORD_BYTES,
                "quality record exceeds wire limit"
            );
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn bounded_line<R: BufRead>(reader: &mut R, max: usize, total: &mut usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        ensure!(!available.is_empty(), "truncated quality worker response");
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |n| n + 1);
        ensure!(
            take <= max.saturating_sub(line.len()),
            "quality worker line exceeds limit"
        );
        ensure!(
            take <= MAX_WIRE_BYTES.saturating_sub(*total),
            "quality worker output exceeds limit"
        );
        let ended = available[take - 1] == b'\n';
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        *total += take;
        if ended {
            return Ok(line);
        }
    }
}

fn parse_reply<R: BufRead>(mut reader: R) -> Result<Option<Vec<QualityRecord>>> {
    let mut total = 0;
    let line = bounded_line(&mut reader, MAX_HEADER_BYTES, &mut total)?;
    let header: Header = serde_json::from_slice(&line)?;
    ensure!(header.version == 2, "unsupported quality worker protocol");
    ensure!(
        header.row_count <= MAX_ROWS,
        "quality worker row count exceeds limit"
    );
    ensure!(
        header.has_quality || header.row_count == 0,
        "quality worker header inconsistent"
    );
    let mut rows = Vec::new();
    let mut ids = std::collections::HashSet::new();
    for _ in 0..header.row_count {
        let line = bounded_line(&mut reader, MAX_RECORD_BYTES, &mut total)?;
        let row: QualityRecord = serde_json::from_slice(&line)?;
        row.validate_worker_record()?;
        ensure!(ids.insert(row.id), "duplicate worker quality record ID");
        rows.push(row);
    }
    ensure!(
        reader.fill_buf()?.is_empty(),
        "trailing quality worker data"
    );
    Ok(header.has_quality.then_some(rows))
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Only call with CapturedInput::path after authentication.
pub fn decode_captured(path: &Path) -> Result<Option<Vec<QualityRecord>>> {
    let folder = tempfile::tempdir().context("create private S-102 worker directory")?;
    let output = folder.path().join("quality.ndjson");
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg(FLAG)
        .arg(path)
        .arg(&output)
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
            .context("start isolated S-102 quality decoder")?,
    );
    let start = Instant::now();
    let status = loop {
        if let Ok(metadata) = fs::metadata(&output) {
            ensure!(
                metadata.len() <= MAX_WIRE_BYTES as u64,
                "quality worker output exceeds limit"
            );
        }
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        ensure!(start.elapsed() < TIMEOUT, "S-102 quality decoder timed out");
        std::thread::sleep(Duration::from_millis(20));
    };
    ensure!(
        status.success(),
        "S-102 quality decoder rejected input or exited abnormally"
    );
    let file = File::open(&output).context("quality worker reply missing")?;
    ensure!(
        file.metadata()?.len() <= MAX_WIRE_BYTES as u64,
        "quality worker output exceeds limit"
    );
    parse_reply(BufReader::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoded(header: Header, rows: &[QualityRecord]) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&header).unwrap();
        bytes.push(b'\n');
        for row in rows {
            bytes.extend(serde_json::to_vec(row).unwrap());
            bytes.push(b'\n');
        }
        bytes
    }
    #[test]
    fn header_rejects_million_rows_before_allocating_records() {
        let data = b"{\"version\":2,\"has_quality\":true,\"row_count\":1000000}\n";
        assert!(parse_reply(&data[..]).is_err());
    }
    #[test]
    fn rejects_oversize_line_and_trailing_data() {
        let header = Header {
            version: 2,
            has_quality: true,
            row_count: 1,
        };
        let mut data = serde_json::to_vec(&header).unwrap();
        data.push(b'\n');
        data.extend(std::iter::repeat_n(b' ', MAX_RECORD_BYTES + 1));
        data.push(b'\n');
        assert!(parse_reply(&data[..]).is_err());
        let mut data = encoded(
            Header {
                row_count: 0,
                ..header
            },
            &[],
        );
        data.push(b'x');
        assert!(parse_reply(&data[..]).is_err());
    }
    #[test]
    fn valid_unicode_and_original_bytes_roundtrip() {
        let mut row = QualityRecord {
            id: 7,
            ..Default::default()
        };
        row.survey_authority = Some("조사".into());
        row.raw_string_bytes
            .insert("surveyAuthority".into(), vec![0xff]);
        row.encoding_warnings
            .push("surveyAuthority: source encoding warning".into());
        let data = encoded(
            Header {
                version: 2,
                has_quality: true,
                row_count: 1,
            },
            &[row],
        );
        let rows = parse_reply(&data[..]).unwrap().unwrap();
        assert_eq!(rows[0].survey_authority.as_deref(), Some("조사"));
        assert_eq!(rows[0].raw_string_bytes["surveyAuthority"], [0xff]);
    }
    #[test]
    fn rejects_excess_raw_keys_and_warnings() {
        let mut row = QualityRecord {
            id: 1,
            ..Default::default()
        };
        row.raw_string_bytes.insert("unexpected".into(), vec![1]);
        let data = encoded(
            Header {
                version: 2,
                has_quality: true,
                row_count: 1,
            },
            &[row],
        );
        assert!(parse_reply(&data[..]).is_err());
        let mut row = QualityRecord {
            id: 1,
            ..Default::default()
        };
        row.encoding_warnings = vec!["warning".into(); 5];
        let data = encoded(
            Header {
                version: 2,
                has_quality: true,
                row_count: 1,
            },
            &[row],
        );
        assert!(parse_reply(&data[..]).is_err());
    }
}
