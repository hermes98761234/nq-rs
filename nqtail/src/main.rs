//! nqtail — follow output of running / queued nq jobs.
//!
//! Usage:
//!   nqtail [JOBIDs...]    Follow output of one or more jobs.
//!   nqtail -a             Follow all jobs (default when no JOBIDs given).
//!   nqtail -n             No wait: if a job is already done, print output and exit.
//!   nqtail -q             Quiet: print one line per job (the job id), don't follow.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;

use nq_core::dir::QueueDir;
use nq_core::job::{self, JobId};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// nqtail — follow output of running nq jobs
#[derive(Parser, Debug)]
#[command(name = "nqtail", version, about)]
struct Cli {
    /// Follow all jobs (default when no JOBIDs are given)
    #[arg(short = 'a')]
    all: bool,

    /// No wait: if a job is already done, print its output and exit immediately
    #[arg(short = 'n')]
    no_wait: bool,

    /// Quiet: print one line per job id and exit; don't follow output
    #[arg(short = 'q')]
    quiet: bool,

    /// Job IDs to follow. If empty and not -a, lists jobs in NQDIR.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    job_ids: Vec<String>,
}

// ---------------------------------------------------------------------------
// Cross-platform file change notification
// ---------------------------------------------------------------------------

/// A per-file watcher that blocks until the file is written to or closed.
#[cfg(target_os = "linux")]
mod notify {
    use std::fs::File;
    use std::io;
    use std::os::fd::RawFd;
    use std::path::Path;

    use libc::{IN_CLOSE_WRITE, IN_MODIFY};

    pub struct FileWatcher {
        inotify_fd: RawFd,
        watch_fd: i32,
        buf: [u8; 4096],
    }

    impl FileWatcher {
        pub fn new(_file: &File, path: &Path) -> io::Result<Self> {
            let inotify_fd = unsafe { libc::inotify_init() };
            if inotify_fd == -1 {
                return Err(io::Error::last_os_error());
            }

            let cpath = std::ffi::CString::new(path.to_string_lossy().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path"))?;

            let watch_fd = unsafe {
                libc::inotify_add_watch(inotify_fd, cpath.as_ptr(), IN_MODIFY | IN_CLOSE_WRITE)
            };
            if watch_fd == -1 {
                let err = io::Error::last_os_error();
                unsafe { libc::close(inotify_fd) };
                return Err(err);
            }

            Ok(Self {
                inotify_fd,
                watch_fd,
                buf: [0u8; 4096],
            })
        }

        pub fn wait_for_change(&mut self) -> io::Result<()> {
            let n = unsafe {
                libc::read(
                    self.inotify_fd,
                    self.buf.as_mut_ptr() as *mut libc::c_void,
                    self.buf.len(),
                )
            };
            if n == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for FileWatcher {
        fn drop(&mut self) {
            unsafe {
                if self.watch_fd >= 0 {
                    libc::inotify_rm_watch(self.inotify_fd, self.watch_fd);
                }
                if self.inotify_fd >= 0 {
                    libc::close(self.inotify_fd);
                }
            }
        }
    }
}

/// kqueue-based watcher for macOS, FreeBSD, and the other BSDs.
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod notify {
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::path::Path;

    use libc::{EVFILT_VNODE, EV_ADD, EV_CLEAR, NOTE_WRITE, NOTE_EXTEND};

    pub struct FileWatcher {
        kq: RawFd,
        changelist: [libc::kevent; 1],
        eventlist: [libc::kevent; 1],
    }

    impl FileWatcher {
        pub fn new(file: &File, _path: &Path) -> io::Result<Self> {
            let kq = unsafe { libc::kqueue() };
            if kq == -1 {
                return Err(io::Error::last_os_error());
            }

            let fd = file.as_raw_fd();
            let changelist = [libc::kevent {
                ident: fd as libc::uintptr_t,
                filter: EVFILT_VNODE as i16,
                flags: EV_ADD | EV_CLEAR,
                fflags: (NOTE_WRITE | NOTE_EXTEND) as u32,
                data: 0,
                udata: std::ptr::null_mut(),
            }];

            // Register the event.
            let ret = unsafe {
                libc::kevent(
                    kq,
                    changelist.as_ptr(),
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if ret == -1 {
                let err = io::Error::last_os_error();
                unsafe { libc::close(kq) };
                return Err(err);
            }

            Ok(Self {
                kq,
                changelist,
                eventlist: [libc::kevent {
                    ident: 0,
                    filter: 0,
                    flags: 0,
                    fflags: 0,
                    data: 0,
                    udata: std::ptr::null_mut(),
                }],
            })
        }

        pub fn wait_for_change(&mut self) -> io::Result<()> {
            let ret = unsafe {
                libc::kevent(
                    self.kq,
                    std::ptr::null(),
                    0,
                    self.eventlist.as_mut_ptr(),
                    1,
                    std::ptr::null(),
                )
            };
            if ret == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for FileWatcher {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.kq);
            }
        }
    }
}

/// Fallback: simple polling sleep for Windows / unknown platforms.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)))]
mod notify {
    use std::fs::File;
    use std::io;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    pub struct FileWatcher;

    impl FileWatcher {
        pub fn new(_file: &File, _path: &Path) -> io::Result<Self> {
            Ok(Self)
        }

        pub fn wait_for_change(&mut self) -> io::Result<()> {
            thread::sleep(Duration::from_millis(250));
            Ok(())
        }
    }
}

use notify::FileWatcher;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the list of `JobId`s to follow from CLI args and flags.
fn resolve_job_ids(qd: &QueueDir, args: &[String], _all: bool) -> Result<Vec<JobId>> {
    if !args.is_empty() {
        // Parse explicit job IDs from the command line.
        let mut ids = Vec::with_capacity(args.len());
        for a in args {
            let id = JobId::from_filename(a)
                .or_else(|| JobId::from_filename(&format!(",{a}")))
                .context(format!("invalid job id: {a}"))?;
            ids.push(id);
        }
        return Ok(ids);
    }
    // No explicit IDs: list all jobs from the queue directory.
    qd.list_jobs().context("list jobs")
}

/// Follow a single job file, printing new output to stdout as it arrives.
fn follow_job(qd: &QueueDir, id: &JobId, no_wait: bool) -> Result<()> {
    let job_path = qd.path.join(id.filename());
    let mut file = File::open(&job_path).with_context(|| format!("open {id}"))?;

    // Seek past the exec line and any output that already exists.
    file.seek(SeekFrom::End(0))?;

    let running = job::is_running(&file);

    if !running && no_wait {
        // Done already and user said -n: nothing new to show.
        return Ok(());
    }

    if !running {
        // Done already but user didn't say -n: dump whatever output exists
        // (might be empty if the job finished before writing anything).
        io::copy(&mut file, &mut io::stdout()).ok();
        return Ok(());
    }

    // Job is running (or queued) — follow it until it finishes.
    let mut watcher = FileWatcher::new(&file, &job_path)?;

    loop {
        // Block until the file changes.
        watcher.wait_for_change()?;

        // Read whatever new data is available.
        let mut buf = [0u8; 8192];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,      // no more data right now
                Ok(n) => {
                    io::stdout().write_all(&buf[..n])?;
                    io::stdout().flush()?;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e).context(format!("read {id}")),
            }
        }

        // Check if the job has finished.
        if !job::is_running(&file) {
            // One final read for anything that arrived between the last
            // notification and the lock check.
            io::copy(&mut file, &mut io::stdout()).ok();
            break;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nqtail: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let qd = QueueDir::open()?;

    let ids = resolve_job_ids(&qd, &cli.job_ids, cli.all)?;

    if ids.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    // Quiet mode: just print one line per job id, then exit.
    if cli.quiet {
        for id in &ids {
            println!("{id}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Follow each job in order.
    for id in &ids {
        follow_job(&qd, &id, cli.no_wait)?;
    }

    Ok(ExitCode::SUCCESS)
}
