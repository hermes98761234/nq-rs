//! nq — simple job queue runner.
//!
//! Queues a shell command to run once all earlier queued commands have completed.
//!
//! Usage:
//!   nq [-c] [-q] CMD...           Queue a command (default mode).
//!   nq [-c] -w [JOBIDs...]        Wait for one or more jobs to complete.
//!   nq [-c] -t [JOBIDs...]        Test whether jobs are done.
//!
//! The `-c` flag (compact) prints only the job count instead of full IDs.

use std::io::Write;
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

use anyhow::{bail, Context, Result};
use clap::Parser;

use nq_core::dir::QueueDir;
use nq_core::exec::write_exec_line;
use nq_core::job::{self, JobId};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// nq - simple job queue. Runs a command in the background once all earlier
/// queued commands have completed.
#[derive(Parser, Debug)]
#[command(name = "nq", version, about)]
struct Cli {
    /// Wait mode: wait for one or more jobs to finish (default: all jobs)
    #[arg(short = 'w')]
    wait: bool,

    /// Test mode: check whether jobs are done, exit 0/1 accordingly
    #[arg(short = 't')]
    test: bool,

    /// Compact output: only print a count of job IDs
    #[arg(short = 'c')]
    compact: bool,

    /// Quiet mode: do not print the job ID on stderr when queueing
    #[arg(short = 'q')]
    quiet: bool,

    /// In wait/test mode: optional list of JobIds (no prefix).
    /// In queue mode: the command and its arguments to queue.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parses args into `JobId` values.
fn parse_job_ids(args: &[String]) -> Result<Vec<JobId>> {
    let mut ids = Vec::with_capacity(args.len());
    for a in args {
        let id = JobId::from_filename(a)
            .or_else(|| JobId::from_filename(&format!(",{a}")))
            .context(format!("invalid job id: {a}"))?;
        ids.push(id);
    }
    Ok(ids)
}

/// Mark a file as executable (+x) on Unix.
#[cfg(unix)]
fn set_executable(path: &std::path::Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o111);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Remove the executable bit (-x) on Unix.
#[cfg(unix)]
fn clear_executable(path: &std::path::Path) -> Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() & !0o111);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &std::path::Path) -> Result<()> {
    Ok(()) // no-op on Windows
}

#[cfg(not(unix))]
fn clear_executable(_path: &std::path::Path) -> Result<()> {
    Ok(()) // no-op on Windows
}

/// Return this binary's own path for spawning the daemon worker.
fn self_exe() -> Result<PathBuf> {
    std::env::current_exe().context("cannot get self exe path")
}

// ---------------------------------------------------------------------------
// Queue mode
// ---------------------------------------------------------------------------

/// Queue a command: create the job file, write exec line, spawn daemon child.
fn cmd_queue(qd: &QueueDir, cmd: &[String], quiet: bool) -> Result<()> {
    let id = JobId::new();
    let job_path = qd.path.join(id.filename());

    // Create the job file with an exclusive lock.
    let mut file = qd.create_job(&id)?;

    // Write the exec line.
    let args: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    write_exec_line(&mut file, &args)?;
    file.flush()?;

    // Spawn the daemon worker — it inherits the lock path so it knows
    // which job to run.
    let lock_path = job_path.to_string_lossy().to_string();
    let daemon = Command::new(self_exe()?)
        .env("NQ_INTERNAL_DAEMON_LOCK", &lock_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn daemon worker")?;

    // Detach — the daemon outlives us.
    drop(daemon);

    if !quiet {
        eprintln!("{}", id);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Wait mode
// ---------------------------------------------------------------------------

/// Wait for one or more jobs to finish.
fn cmd_wait(qd: &QueueDir, ids: &[JobId]) -> Result<()> {
    let targets = if ids.is_empty() {
        qd.list_jobs()?
    } else {
        ids.to_vec()
    };

    for id in &targets {
        let file = match qd.open_job(id) {
            Ok(f) => f,
            Err(_) => continue, // already moved/removed by daemon
        };
        if job::is_running(&file) {
            job::wait_for_lock(&file);
        }
        // Remove the executable bit now that the job is done.
        let job_path = qd.path.join(id.filename());
        let _ = clear_executable(&job_path);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Test mode
// ---------------------------------------------------------------------------

/// Test whether one or more jobs are done (exit 0 = all done, exit 1 = not).
fn cmd_test(qd: &QueueDir, ids: &[JobId]) -> Result<bool> {
    let targets = if ids.is_empty() {
        qd.list_jobs()?
    } else {
        ids.to_vec()
    };

    for id in &targets {
        let file = match qd.open_job(id) {
            Ok(f) => f,
            Err(_) => return Ok(false), // missing = not done
        };
        if !job::try_lock_shared(&file) {
            return Ok(false);
        }
        // Successfully got shared lock → job is done. Release and continue.
        job::unlock(&file);
    }

    Ok(true)
}

// ---------------------------------------------------------------------------
// Daemon worker
// ---------------------------------------------------------------------------

/// The daemon worker process, spawned by `cmd_queue`. It executes the queued
/// command once all older jobs have completed.
#[cfg(unix)]
fn run_daemon(qd: &QueueDir, lock_path: &str) -> Result<()> {
    let lock_path = std::path::Path::new(lock_path);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .context("daemon: open lock file")?;

    // Acquire exclusive lock — our job is ready to run.
    job::lock_exclusive(&file);

    // Wait for all older (sorted earlier) jobs to complete.
    let all_jobs = qd.list_jobs()?;
    let our_id = JobId::from_filename(lock_path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
        .context("daemon: cannot parse own job id from filename")?;

    for id in &all_jobs {
        if id >= &our_id {
            break; // only jobs sorted before ours
        }
        let older_file = match qd.open_job(id) {
            Ok(f) => f,
            Err(_) => continue, // already moved/removed, skip
        };
        if job::is_running(&older_file) {
            job::wait_for_lock(&older_file);
        }
    }

    // Read the exec line from the job file.
    let mut reader = BufReader::new(&file);
    let mut exec_line = String::new();
    reader
        .read_line(&mut exec_line)
        .context("daemon: read exec line")?;

    // Seek back to start? No need, we already read it.
    // Drop the reader so we can operate on the file directly.
    drop(reader);

    // Signal start: write separator, set executable bit.
    write!(file, "\n\n")?;
    file.flush()?;
    set_executable(lock_path)?;

    // Redirect stdout/stderr to the job file.
    let fd = file.as_raw_fd();
    // dup2 on Unix
    unsafe {
        libc::dup2(fd, 1); // stdout
        libc::dup2(fd, 2); // stderr
    }

    // The exec line is a shell command — run it via sh -c.
    // Strip the leading "exec " prefix added by write_exec_line.
    let cmd = exec_line.strip_prefix("exec ").unwrap_or(&exec_line).trim();

    // Fork: parent waits, child execs.
    let child_id = unsafe { libc::fork() };

    match child_id {
        -1 => bail!("daemon: fork failed"),
        0 => {
            // Child: exec the command via shell.
            // Re-open /dev/null for stdin since we don't have input.
            let devnull = std::fs::File::open("/dev/null").expect("/dev/null must exist on Unix");
            unsafe {
                libc::dup2(devnull.as_raw_fd(), 0);
            }
            drop(devnull);

            // Exec the command. Use sh -c so the exec line runs as a shell script.
            let c_cmd = std::ffi::CString::new(cmd).unwrap_or_default();
            let sh = std::ffi::CString::new("sh").unwrap();
            let sh_c = std::ffi::CString::new("-c").unwrap();
            let argv: [*const libc::c_char; 4] =
                [sh.as_ptr(), sh_c.as_ptr(), c_cmd.as_ptr(), std::ptr::null()];
            unsafe {
                libc::execvp(sh.as_ptr(), argv.as_ptr());
            }
            // If we get here, exec failed.
            let _ = writeln!(
                std::io::stderr(),
                "daemon: exec failed: {}",
                std::io::Error::last_os_error()
            );
            unsafe {
                libc::_exit(127);
            }
        }
        _ => {
            // Parent: wait for child.
            let mut status = 0;
            unsafe {
                libc::waitpid(child_id, &mut status, 0);
            }

            // Write exit status to the job file.
            let exit_code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                255
            };

            let _ = writeln!(file, "\n\n+ exit={exit_code}");
            file.flush().ok();

            // Move to done/fail dirs if configured.
            if exit_code == 0 {
                let _ = qd.move_to_done(&our_id);
            } else {
                let _ = qd.move_to_fail(&our_id);
            }

            std::process::exit(exit_code);
        }
    }
}

// ---------------------------------------------------------------------------
// Mode: list (default with no args, no -w/-t)
// ---------------------------------------------------------------------------

/// List jobs in the queue.
fn cmd_list(qd: &QueueDir, compact: bool) -> Result<()> {
    let jobs = qd.list_jobs()?;
    if compact {
        println!("{}", jobs.len());
    } else {
        if jobs.is_empty() {
            // Print nothing, just exit.
            return Ok(());
        }
        for id in &jobs {
            println!("{id}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main entrypoint
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nq: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    // Check for daemon mode first — this env var indicates the daemon worker.
    #[cfg(unix)]
    if let Ok(lock_path) = std::env::var("NQ_INTERNAL_DAEMON_LOCK") {
        let qd = QueueDir::open()?;
        run_daemon(&qd, &lock_path)?;
        // run_daemon calls process::exit internally, so we should never get here.
        return Ok(ExitCode::SUCCESS);
    }
    #[cfg(windows)]
    if std::env::var("NQ_INTERNAL_DAEMON_LOCK").is_ok() {
        bail!("daemon mode not supported on Windows");
    }

    let cli = Cli::parse();
    let qd = QueueDir::open()?;

    match (cli.wait, cli.test, cli.args.is_empty()) {
        // Wait mode
        (true, false, _) => {
            let ids = parse_job_ids(&cli.args)?;
            cmd_wait(&qd, &ids)?;
            Ok(ExitCode::SUCCESS)
        }
        // Test mode
        (false, true, _) => {
            let ids = parse_job_ids(&cli.args)?;
            let all_done = cmd_test(&qd, &ids)?;
            if all_done {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
        // No mode flags and no args → list
        (false, false, true) => {
            cmd_list(&qd, cli.compact)?;
            Ok(ExitCode::SUCCESS)
        }
        // No mode flags and args → queue
        (false, false, false) => {
            cmd_queue(&qd, &cli.args, cli.quiet)?;
            Ok(ExitCode::SUCCESS)
        }
        // Invalid combinations (shouldn't happen with clap but just in case)
        (true, true, _) => {
            bail!("cannot use both -w and -t together");
        }
    }
}
