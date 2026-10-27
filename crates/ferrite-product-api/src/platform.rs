//! Platform services product modules use directly. The engine's `Platform`
//! (`ferrite_engine::platform`) provides the implementations: the system
//! clock, files in the application data folder or records in memory.
use std::io;
use std::time::SystemTime;

/// The time source.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
    /// [`Clock::now`] as UTC.
    fn utc(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from(self.now())
    }
    /// Seconds since the Unix epoch (negative before it).
    fn unix_seconds(&self) -> i64 {
        match self.now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        }
    }
}

/// One persisted per-user record (e.g. the S-124 NAVWARN user state).
pub trait UserRecord: Send + Sync {
    /// The record, None when it was never written; at most `limit + 1` bytes.
    fn read(&self, limit: u64) -> io::Result<Option<Vec<u8>>>;
    /// Replace the record atomically.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;
    /// Keep an unreadable record for inspection (renamed with `suffix`).
    fn set_aside(&self, suffix: &str);
    /// Where the record lives, for messages.
    fn describe(&self) -> String;
    /// Hold the record exclusively across a read, validate and write
    /// transaction; dropping the guard releases it. None: the record needs no
    /// lock (its single owner serializes access, e.g. memory records).
    fn lock(&self) -> io::Result<Option<RecordLock>> {
        Ok(None)
    }
}

/// Guard of [`UserRecord::lock`] (an OS file lock for native records).
pub type RecordLock = Box<dyn std::any::Any + Send>;
