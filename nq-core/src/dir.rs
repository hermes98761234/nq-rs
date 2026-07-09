//! Queue directory — resolves and manages the spool directory for job files.
//!
//! Uses `NQDIR` (default `.`), `NQDONEDIR` (optional), and `NQFAILDIR` (optional)
//! environment variables, matching the original nq semantics.

use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use fs2::FileExt as FlockExt;

use crate::job::JobId;

/// Error type for queue directory operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse job filename: {0}")]
    Parse(String),
    #[error("Job already exists: {0}")]
    Exists(String),
}

/// A managed queue directory backed by `NQDIR` (and optionally `NQDONEDIR`/`NQFAILDIR`).
pub struct QueueDir {
    /// Path to the main queue directory.
    pub path: PathBuf,
    /// `File` handle opened on the queue directory (for `fsync` and `as_raw_fd`).
    dir_file: File,
    /// Optional path to the "done" directory where completed jobs are moved.
    pub done_path: Option<PathBuf>,
    /// Optional path to the "fail" directory where failed jobs are moved.
    pub fail_path: Option<PathBuf>,
}

impl QueueDir {
    /// Open (or create) the queue directory from environment variables.
    ///
    /// - `NQDIR` (default `.`): the spool directory
    /// - `NQDONEDIR` (optional): where completed (exit 0) jobs are moved
    /// - `NQFAILDIR` (optional): where failed (exit != 0) jobs are moved
    pub fn open() -> Result<Self, Error> {
        let path = std::env::var("NQDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));

        fs::create_dir_all(&path)?;
        let dir_file = File::open(&path)?;

        let done_path = std::env::var("NQDONEDIR").map(PathBuf::from).ok();
        if let Some(ref p) = done_path {
            fs::create_dir_all(p)?;
        }

        let fail_path = std::env::var("NQFAILDIR").map(PathBuf::from).ok();
        if let Some(ref p) = fail_path {
            fs::create_dir_all(p)?;
        }

        Ok(Self {
            path,
            dir_file,
            done_path,
            fail_path,
        })
    }

    /// Open a `QueueDir` at a specific path (for testing without env vars).
    pub fn open_at<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;
        let dir_file = File::open(&path)?;
        Ok(Self {
            path,
            dir_file,
            done_path: None,
            fail_path: None,
        })
    }

    /// Return the raw file descriptor of the queue directory.
    #[cfg(unix)]
    pub fn dirfd(&self) -> RawFd {
        self.dir_file.as_raw_fd()
    }

    /// Return the raw handle of the queue directory (Windows).
    #[cfg(windows)]
    pub fn dirfd(&self) -> u64 {
        0 // placeholder — not used on Windows
    }

    /// Create a new job file.
    ///
    /// 1. Writes a temp file `.TIMESTAMP.PID` with exclusive flock.
    /// 2. Renames it to `,TIMESTAMP.PID`.
    /// 3. `fsync`s the directory to commit the rename.
    ///
    /// Returns the opened file handle.
    pub fn create_job(&self, id: &JobId) -> Result<File, Error> {
        let temp_path = self.path.join(id.temp_filename());
        let final_path = self.path.join(id.filename());

        if final_path.exists() {
            return Err(Error::Exists(final_path.display().to_string()));
        }

        let mut opts = OpenOptions::new();
        opts.create_new(true).write(true).read(true);
        #[cfg(unix)]
        opts.mode(0o666);
        let file = opts.open(&temp_path)?;

        // Acquire exclusive lock before renaming.
        file.lock_exclusive()?;

        // Rename .TIMESTAMP.PID → ,TIMESTAMP.PID
        fs::rename(&temp_path, &final_path)?;

        // fsync the directory to commit the rename.
        self.dir_file.sync_all()?;

        Ok(file)
    }

    /// List all job files (`,*`) in the queue directory, sorted by filename
    /// (which is chronological by timestamp + pid).
    pub fn list_jobs(&self) -> Result<Vec<JobId>, Error> {
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(',') {
                if let Some(id) = JobId::from_filename(&name) {
                    jobs.push(id);
                }
            }
        }
        jobs.sort();
        Ok(jobs)
    }

    /// Open an existing job file for reading and writing (append mode).
    pub fn open_job(&self, id: &JobId) -> Result<File, Error> {
        let path = self.path.join(id.filename());
        let file = OpenOptions::new().read(true).append(true).open(&path)?;
        Ok(file)
    }

    /// Move a job file from the queue directory to the "done" directory.
    ///
    /// Does nothing if `NQDONEDIR` was not set.
    pub fn move_to_done(&self, id: &JobId) -> Result<(), Error> {
        if let Some(ref done) = self.done_path {
            let src = self.path.join(id.filename());
            let dst = done.join(id.filename());
            fs::rename(&src, &dst)?;
        }
        Ok(())
    }

    /// Move a job file from the queue directory to the "fail" directory.
    ///
    /// Does nothing if `NQFAILDIR` was not set.
    pub fn move_to_fail(&self, id: &JobId) -> Result<(), Error> {
        if let Some(ref fail) = self.fail_path {
            let src = self.path.join(id.filename());
            let dst = fail.join(id.filename());
            fs::rename(&src, &dst)?;
        }
        Ok(())
    }

    /// Remove (unlink) a job file from the queue directory.
    pub fn remove_job(&self, id: &JobId) -> Result<(), Error> {
        let path = self.path.join(id.filename());
        fs::remove_file(&path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::job;
    use std::io::Write;

    #[test]
    fn test_create_and_list_job() {
        let dir = tempfile::tempdir().unwrap();
        let qd = QueueDir::open_at(dir.path()).unwrap();
        let id = JobId::new();

        let mut file = qd.create_job(&id).unwrap();
        writeln!(file, "exec test").unwrap();
        // Unlock so the write is visible.
        job::unlock(&file);

        let jobs = qd.list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0], id);
    }

    #[test]
    fn test_list_jobs_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let qd = QueueDir::open_at(dir.path()).unwrap();

        let id_a = JobId {
            timestamp_ms: 100,
            pid: 1,
        };
        let id_b = JobId {
            timestamp_ms: 200,
            pid: 2,
        };
        let id_c = JobId {
            timestamp_ms: 150,
            pid: 3,
        };

        // Create in non-sorted order.
        let mut f = qd.create_job(&id_b).unwrap();
        writeln!(f, "exec b").unwrap();
        job::unlock(&f);

        let mut f = qd.create_job(&id_a).unwrap();
        writeln!(f, "exec a").unwrap();
        job::unlock(&f);

        let mut f = qd.create_job(&id_c).unwrap();
        writeln!(f, "exec c").unwrap();
        job::unlock(&f);

        let jobs = qd.list_jobs().unwrap();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0], id_a);
        assert_eq!(jobs[1], id_c);
        assert_eq!(jobs[2], id_b);
    }

    #[test]
    fn test_open_job() {
        let dir = tempfile::tempdir().unwrap();
        let qd = QueueDir::open_at(dir.path()).unwrap();
        let id = JobId::new();

        let mut file = qd.create_job(&id).unwrap();
        writeln!(file, "exec hello").unwrap();
        job::unlock(&file);

        // Re-open and read.
        let _opened = qd.open_job(&id).unwrap();
        let content = std::fs::read_to_string(dir.path().join(id.filename())).unwrap();
        assert!(content.starts_with("exec hello"));
    }

    #[test]
    fn test_remove_job() {
        let dir = tempfile::tempdir().unwrap();
        let qd = QueueDir::open_at(dir.path()).unwrap();
        let id = JobId::new();

        let mut f = qd.create_job(&id).unwrap();
        writeln!(f, "exec test").unwrap();
        job::unlock(&f);

        assert!(qd.list_jobs().unwrap().len() == 1);
        qd.remove_job(&id).unwrap();
        assert!(qd.list_jobs().unwrap().is_empty());
    }

    #[test]
    fn test_move_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let done_dir = dir.path().join("done");
        fs::create_dir_all(&done_dir).unwrap();

        // Custom QueueDir with done_path set.
        let qd = QueueDir {
            path: dir.path().to_path_buf(),
            dir_file: File::open(dir.path()).unwrap(),
            done_path: Some(done_dir.clone()),
            fail_path: None,
        };

        let id = JobId::new();
        let mut f = qd.create_job(&id).unwrap();
        writeln!(f, "exec test").unwrap();
        job::unlock(&f);

        qd.move_to_done(&id).unwrap();
        assert!(!dir.path().join(id.filename()).exists());
        assert!(done_dir.join(id.filename()).exists());
    }

    #[test]
    fn test_move_to_fail() {
        let dir = tempfile::tempdir().unwrap();
        let fail_dir = dir.path().join("fail");
        fs::create_dir_all(&fail_dir).unwrap();

        let qd = QueueDir {
            path: dir.path().to_path_buf(),
            dir_file: File::open(dir.path()).unwrap(),
            done_path: None,
            fail_path: Some(fail_dir.clone()),
        };

        let id = JobId::new();
        let mut f = qd.create_job(&id).unwrap();
        writeln!(f, "exec test").unwrap();
        job::unlock(&f);

        qd.move_to_fail(&id).unwrap();
        assert!(!dir.path().join(id.filename()).exists());
        assert!(fail_dir.join(id.filename()).exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_dirfd_raw() {
        let dir = tempfile::tempdir().unwrap();
        let qd = QueueDir::open_at(dir.path()).unwrap();
        // dirfd() should return a valid fd (≥ 0).
        assert!(qd.dirfd() >= 0);
    }
}
