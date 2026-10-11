//! Hidden diagnostic only; bounded ordered-motion proof, no device event claim.
use anyhow::{ensure, Result};
#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct Counts {
    pub deferred: u64,
    pub rebuild_attempts: u64,
    pub flush_attempts: u64,
    pub failed_flushes: u64,
}
#[derive(serde::Serialize)]
struct Row {
    frame: usize,
    per_event_camera_bits: [[u64; 12]; 8],
    event_count: usize,
    before: Counts,
    after: Counts,
    final_camera_bits: [u64; 12],
}
pub(crate) struct Audit {
    pub capture_enabled: bool,
    pub replay_index: Option<usize>,
    captures: Vec<serde_json::Value>,
    pub burst: usize,
    pub counts: Counts,
    rows: Vec<Row>,
    events: [[u64; 12]; 8],
    event_count: usize,
    before: Counts,
}
impl Audit {
    pub fn new(value: &std::ffi::OsStr, capture_enabled: bool) -> Result<Self> {
        let burst = match value.to_str() {
            Some("4") => 4,
            Some("8") => 8,
            _ => anyhow::bail!("Burst diagnostic requires exact 4 or 8"),
        };
        let rows = Vec::with_capacity(500);
        ensure!(
            rows.capacity()
                .checked_mul(std::mem::size_of::<Row>())
                .is_some_and(|n| n <= 1024 * 1024),
            "Burst proof capacity exceeds1MiB"
        );
        Ok(Self {
            capture_enabled,
            replay_index: None,
            captures: Vec::with_capacity(20),
            burst,
            counts: Counts::default(),
            rows,
            events: [[0; 12]; 8],
            event_count: 0,
            before: Counts::default(),
        })
    }
    pub fn begin(&mut self) {
        self.events = [[0; 12]; 8];
        self.event_count = 0;
        self.before = self.counts;
    }
    pub fn motion(&mut self, bits: [u64; 12]) -> Result<()> {
        ensure!(
            self.event_count < self.burst && self.event_count < 8,
            "Too many burst motions"
        );
        self.events[self.event_count] = bits;
        self.event_count += 1;
        Ok(())
    }
    pub fn complete(&mut self, frame: usize, bits: [u64; 12]) -> Result<()> {
        ensure!(
            frame == self.rows.len() && frame < 500 && self.event_count == self.burst,
            "Incomplete ordered burst proof"
        );
        self.rows.push(Row {
            frame,
            per_event_camera_bits: self.events,
            event_count: self.event_count,
            before: self.before,
            after: self.counts,
            final_camera_bits: bits,
        });
        Ok(())
    }
    pub fn verify_replay(&self, frame: usize, bits: [u64; 12]) -> Result<()> {
        ensure!(
            frame < 500
                && self.replay_index == Some(frame)
                && self.rows.len() == 500
                && self.event_count == self.burst,
            "Replay ownership/length mismatch"
        );
        let old = &self.rows[frame];
        if old.per_event_camera_bits != self.events || old.final_camera_bits != bits {
            let names = [
                "zoom",
                "pan_x",
                "pan_y",
                "geo_min_x",
                "geo_min_y",
                "geo_max_x",
                "geo_max_y",
                "scale_x",
                "scale_y",
                "viewport_x_f32",
                "viewport_y_f32",
                "viewport_width_height_f32",
            ];
            for event in 0..self.burst {
                for (field, name) in names.iter().enumerate() {
                    let expected = old.per_event_camera_bits[event][field];
                    let actual = self.events[event][field];
                    if expected != actual {
                        anyhow::bail!("Replay camera/event state not bit-exact: frame={frame} phase=motion event={event} field={name} expected_bits={expected:#018x} actual_bits={actual:#018x} expected_camera={:?} actual_camera={:?}", old.per_event_camera_bits[event], self.events[event]);
                    }
                }
            }
            for (field, name) in names.iter().enumerate() {
                let expected = old.final_camera_bits[field];
                let actual = bits[field];
                if expected != actual {
                    anyhow::bail!("Replay camera/event state not bit-exact: frame={frame} phase=final field={name} expected_bits={expected:#018x} actual_bits={actual:#018x} expected_camera={:?} actual_camera={bits:?}", old.final_camera_bits);
                }
            }
            anyhow::bail!(
                "Replay camera/event state not bit-exact outside active events: frame={frame}"
            );
        }
        Ok(())
    }
    pub fn selected(frame: usize) -> bool {
        [
            0, 33, 66, 99, 100, 133, 166, 199, 200, 233, 266, 299, 300, 333, 366, 399, 400, 433,
            466, 499,
        ]
        .contains(&frame)
    }
    pub fn captured(&mut self, frame: usize, path: &std::path::Path) -> Result<()> {
        use sha2::{Digest, Sha256};
        ensure!(
            Self::selected(frame) && frame < self.rows.len() && self.captures.len() < 20,
            "Capture key/cap mismatch"
        );
        let mut files = std::collections::BTreeMap::new();
        let mut stack = vec![path.to_path_buf()];
        let mut total = 0u64;
        while let Some(dir) = stack.pop() {
            for item in std::fs::read_dir(&dir)? {
                let p = item?.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    let size = std::fs::metadata(&p)?.len();
                    total = total
                        .checked_add(size)
                        .ok_or_else(|| anyhow::anyhow!("Capture size overflow"))?;
                    ensure!(
                        total <= 128 * 1024 * 1024,
                        "Capture file payload exceeds128MiB"
                    );
                    use std::io::Read;
                    let mut file = std::fs::File::open(&p)?;
                    let mut hash = Sha256::new();
                    let mut buffer = [0u8; 65536];
                    let mut read = 0u64;
                    loop {
                        let n = file.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        read = read
                            .checked_add(n as u64)
                            .ok_or_else(|| anyhow::anyhow!("Capture length overflow"))?;
                        ensure!(read <= size, "Captured output grew while hashing");
                        hash.update(&buffer[..n]);
                    }
                    ensure!(read == size, "Captured output shrank while hashing");
                    files.insert(
                        p.strip_prefix(path)?.to_string_lossy().into_owned(),
                        format!("{:x}", hash.finalize()),
                    );
                }
            }
        }
        ensure!(
            files.contains_key("frame.png")
                && files.contains_key(if ferrite_wgpu::audit_digest_only_enabled() {
                    "instructions.sha256.json"
                } else {
                    "instructions.bin"
                })
                && files.contains_key("snapshot.json")
                && files.contains_key("gpu/metadata.json"),
            "Missing captured outputs"
        );
        if ferrite_wgpu::audit_digest_only_enabled() {
            validate_digest_buffers(path, &files)?;
        }
        self.captures.push(serde_json::json!({"frame":frame,"path":path,"files_sha256":files,"camera_bits":self.rows[frame].final_camera_bits}));
        Ok(())
    }
    pub fn export_capture(&self, path: &std::path::Path) -> Result<()> {
        ensure!(
            self.replay_index == Some(500) && self.captures.len() == 20,
            "Incomplete500 replay/20pose capture"
        );
        std::fs::write(
            path.join("flat-event-burst-capture.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"terminal_passed":true,"process_id":std::process::id(),"timing_excluded_readbacks":true,"replayed_callbacks":500,"burst":self.burst,"poses":self.captures,"scope":"Actual chronological hidden second500 callback replay; all per-event/final camera bits match timed transcript.20 after-render selected fulloutput readbacks. No positive-pick/GPU-uniform-readback or physicalFPS claim."}),
            )?,
        )?;
        Ok(())
    }
    pub fn export(&self, path: &std::path::Path) -> Result<()> {
        ensure!(self.rows.len() == 500, "Incomplete burst callback proof");
        std::fs::write(
            path.join("flat-event-burst.json"),
            serde_json::to_vec_pretty(
                &serde_json::json!({"burst":self.burst,"rows":self.rows,"scope":"Synthetic ordered camera helper calls within actual Redraw callback. Not physical OS input coalescing. Every event f64/f32 bit transcript; actual scene rebuild/flush counters. No readback or GPU/pixel certification in this file."}),
            )?,
        )?;
        Ok(())
    }
}

/// Hash-only captures still require the complete nine original byte streams,
/// including explicit represented-byte bounds. Small output files alone are not
/// evidence that a renderer supplied all geometry.
fn validate_digest_buffers(
    path: &std::path::Path,
    files: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    use std::io::Read;
    let mut represented = 0u64;
    for name in [
        "instructions",
        "gpu/area-vertices",
        "gpu/area-indices",
        "gpu/line-vertices",
        "gpu/line-indices",
        "gpu/symbol-vertices",
        "gpu/symbol-indices",
        "gpu/pattern-vertices",
        "gpu/pattern-indices",
    ] {
        let key = format!("{name}.sha256.json");
        ensure!(
            files.contains_key(&key),
            "Missing full-stream digest: {name}"
        );
        ensure!(
            !files.contains_key(&format!("{name}.bin")),
            "Hash-only capture unexpectedly retained raw data"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(path.join(key))?
            .take(1025)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024, "Oversized digest descriptor");
        let record: serde_json::Value = serde_json::from_slice(&bytes)?;
        let length = record["byte_len"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Invalid represented byte count"))?;
        let hash = record["sha256"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing digest"))?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && record["raw_buffer_retained"] == false,
            "Invalid complete-stream digest descriptor"
        );
        represented = represented
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("Represented size overflow"))?;
        ensure!(
            represented <= 128 * 1024 * 1024,
            "Represented capture buffers exceed128MiB"
        );
    }
    Ok(())
}

#[cfg(test)]
mod ordered_replay_controls {
    use super::Audit;
    #[test]
    fn ordered_camera_replay_rejects_middle_event_change_and_reordering() {
        for burst in [4, 8] {
            let value = burst.to_string();
            let mut audit = Audit::new(value.as_ref(), false).unwrap();
            for frame in 0..500 {
                audit.begin();
                for event in 0..burst {
                    audit
                        .motion([((frame * 8 + event) as f64).to_bits(); 12])
                        .unwrap();
                }
                audit
                    .complete(frame, [((frame * 8 + burst - 1) as f64).to_bits(); 12])
                    .unwrap();
            }
            audit.replay_index = Some(100);
            audit.begin();
            for event in 0..burst {
                audit
                    .motion([((100 * 8 + event) as f64).to_bits(); 12])
                    .unwrap();
            }
            let final_bits = [((100 * 8 + burst - 1) as f64).to_bits(); 12];
            assert!(audit.verify_replay(100, final_bits).is_ok());
            audit.events.swap(0, 1);
            assert!(audit.verify_replay(100, final_bits).is_err());
            audit.events.swap(0, 1);
            audit.events[burst / 2][5] ^= 1;
            let error = audit
                .verify_replay(100, final_bits)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("frame=100")
                    && error.contains("field=geo_max_x")
                    && error.contains("phase=motion")
            );
            audit.replay_index = Some(500);
            assert!(audit.verify_replay(500, final_bits).is_err());
            assert!(audit.complete(500, final_bits).is_err());
        }
    }
}
