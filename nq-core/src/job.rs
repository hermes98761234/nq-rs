//! Job ID — a unique identifier for a queued job, matching the original nq format `,TIMESTAMP.PID`.
//!
//! Timestamp is milliseconds since Unix epoch (hex), PID is the process ID (decimal).
//! Temp files use `.TIMESTAMP.PID` before being renamed to `,TIMESTAMP.PID`.

use std::fmt;
use std::fs::File;
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;

/// A job identifier matching the `,TIMESTAMP.PID` naming convention from the original nq.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId {
    /// Milliseconds since Unix epoch.
    pub timestamp_ms: u64,
    /// Process ID that created the job.
    pub pid: u32,
}

impl JobId {
    /// Create a new `JobId` from the current system time and the current process ID.
    pub fn new() -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_millis() as u64;
        let pid = std::process::id();
        Self { timestamp_ms, pid }
    }

    /// Parse a `JobId` from a filename string.
    ///
    /// Accepts both the comma-prefixed form (`,TIMESTAMP.PID`) and the
    /// dot-prefixed temp form (`.TIMESTAMP.PID`).
    pub fn from_filename(s: &str) -> Option<Self> {
        let s = s.strip_prefix(',').or_else(|| s.strip_prefix('.'))?;
        let (ts_hex, pid_str) = s.split_once('.')?;
        let timestamp_ms = u64::from_str_radix(ts_hex, 16).ok()?;
        let pid = pid_str.parse::<u32>().ok()?;
        Some(Self { timestamp_ms, pid })
    }

    /// Return the job filename (`,TIMESTAMP.PID`).
    pub fn filename(&self) -> String {
        format!(",{:011x}.{}", self.timestamp_ms, self.pid)
    }

    /// Return the temp filename (`.TIMESTAMP.PID`), used before the job is
    /// fully created and locked.
    pub fn temp_filename(&self) -> String {
        format!(".{:011x}.{}", self.timestamp_ms, self.pid)
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ",{:011x}.{}", self.timestamp_ms, self.pid)
    }
}

// ---------------------------------------------------------------------------
// Flock helpers
// ---------------------------------------------------------------------------

/// Check whether a job file at `fd` is still locked (i.e., the job is running).
///
/// Returns `true` if the file has an exclusive lock held by another process.
pub fn is_running(file: &File) -> bool {
    file.try_lock_shared().is_err()
}

/// Block until the shared lock on `fd` can be acquired (i.e., wait for the
/// running job to finish).
pub fn wait_for_lock(file: &File) {
    // `lock_shared` blocks until the exclusive lock is released.
    let _ = file.lock_shared();
    // Immediately unlock so the caller can use the fd freely.
    let _ = file.unlock();
}

/// Try to acquire a shared (read) lock on `fd`. Returns `true` if the lock
/// was acquired, `false` if another process holds an exclusive lock.
pub fn try_lock_shared(file: &File) -> bool {
    file.try_lock_shared().is_ok()
}

/// Acquire an exclusive lock on `fd`. Blocks until the lock is obtained.
pub fn lock_exclusive(file: &File) {
    let _ = file.lock_exclusive();
}

/// Release any lock held on `fd`.
pub fn unlock(file: &File) {
    let _ = file.unlock();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_filename_comma() {
        let id = JobId::from_filename(",14f6f3034f8.17035").unwrap();
        assert_eq!(id.timestamp_ms, 0x14f6f3034f8);
        assert_eq!(id.pid, 17035);
    }

    #[test]
    fn test_from_filename_dot() {
        let id = JobId::from_filename(".14f6f3034f8.17035").unwrap();
        assert_eq!(id.timestamp_ms, 0x14f6f3034f8);
        assert_eq!(id.pid, 17035);
    }

    #[test]
    fn test_from_filename_invalid() {
        assert!(JobId::from_filename("").is_none());
        assert!(JobId::from_filename("abc").is_none());
        assert!(JobId::from_filename(",abc.def").is_none());
        assert!(JobId::from_filename(",14f6f3034f8.").is_none());
    }

    #[test]
    fn test_filename_roundtrip() {
        let id = JobId::from_filename(",14f6f3034f8.17035").unwrap();
        assert_eq!(id.filename(), ",14f6f3034f8.17035");
        assert_eq!(id.temp_filename(), ".14f6f3034f8.17035");
    }

    #[test]
    fn test_new_job_id_is_valid() {
        let id = JobId::new();
        assert!(id.timestamp_ms > 0);
        assert!(id.pid > 0);
        // Can re-parse our own filename.
        let parsed = JobId::from_filename(&id.filename()).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_ordering() {
        let a = JobId { timestamp_ms: 100, pid: 1 };
        let b = JobId { timestamp_ms: 100, pid: 2 };
        let c = JobId { timestamp_ms: 200, pid: 1 };

        assert!(a < b);   // same ts, different pid
        assert!(b < c);   // different ts
        assert!(a < c);
        // Same ts + same pid → equal
        assert_eq!(a, JobId { timestamp_ms: 100, pid: 1 });
    }

    #[test]
    fn test_zero_padded() {
        let id = JobId { timestamp_ms: 0xabc, pid: 42 };
        assert_eq!(id.filename(), ",00000000abc.42");
    }

    #[test]
    fn test_display() {
        let id = JobId { timestamp_ms: 0x14f6f3034f8, pid: 17035 };
        assert_eq!(format!("{id}"), ",14f6f3034f8.17035");
    }
}
