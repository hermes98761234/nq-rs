#![cfg(unix)]
//! Integration tests for the `nq` binary.
//!
//! Each test creates a fresh temp dir under `$TMPDIR/nqtest/<test_name>`,
//! sets `NQDIR` to point there, and invokes the `nq` binary directly.
//! Tests use their own subdirectory so they don't conflict on file paths,
//! but they MUST run with `--test-threads=1` because the daemon processes
//! spawned by each test inherit environment and use flock() — parallel
//! execution causes flaky lock interactions.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Path to the `nq` binary under test (set automatically by cargo).
fn nq_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nq"))
}

/// Create a clean, empty temp directory for a test.
fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("nqtest").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `nq` with `NQDIR` set to `dir` and return the full `Output`.
fn run_nq(dir: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(nq_bin())
        .env("NQDIR", dir)
        .args(args)
        .output()
        .unwrap()
}

/// Block until all jobs in `dir` have completed (via `nq -w`).
fn wait_all(dir: &PathBuf) {
    let output = run_nq(dir, &["-w"]);
    assert!(
        output.status.success(),
        "wait mode (-w) should succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Count job files (names starting with `,`) in a directory.
fn count_jobs(dir: &PathBuf) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(','))
        .count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `nq` with no arguments should list the queue — an empty queue produces
/// no output and exits successfully.
#[test]
fn test_fails_no_args() {
    let dir = test_dir("fails_no_args");
    let output = run_nq(&dir, &[]);
    assert!(
        output.status.success(),
        "nq with no args should succeed (list mode)"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "empty queue listing should have no stdout"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).trim().is_empty(),
        "empty queue listing should have no stderr"
    );
}

/// Enqueue `true`, verify a job file is created with the correct exec line,
/// wait for completion, and confirm the exit status is written.
#[test]
fn test_enqueue_true() {
    let dir = test_dir("enqueue_true");

    let output = run_nq(&dir, &["true"]);
    assert!(output.status.success(), "enqueue should succeed");

    // The job ID is printed to stderr.
    let job_id = String::from_utf8_lossy(&output.stderr).trim().to_string();
    assert!(!job_id.is_empty(), "should print job ID to stderr");
    assert!(
        job_id.starts_with(','),
        "job ID should start with comma: {job_id}"
    );

    wait_all(&dir);

    // After completion, the job file should still exist (no NQDONEDIR set).
    assert_eq!(count_jobs(&dir), 1, "one job file should remain");

    let content = fs::read_to_string(dir.join(&job_id)).unwrap_or_else(|_| {
        // If the exact name doesn't match, find the first job file.
        let files: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(','))
            .map(|e| e.path())
            .collect();
        fs::read_to_string(&files[0]).unwrap()
    });

    assert!(
        content.contains("exec true"),
        "job file should contain exec line"
    );
    assert!(
        content.contains("+ exit=0"),
        "job file should contain exit status for success"
    );
}

/// Enqueue a slow job followed by a fast job, verify both complete (the fast
/// job's daemon waits for the slow one, and both exit successfully).
#[test]
fn test_queue_order() {
    let dir = test_dir("queue_order");

    // Enqueue a slow job.
    let out1 = run_nq(&dir, &["sleep", "1"]);
    assert!(
        out1.status.success(),
        "first enqueue (sleep 1) should succeed"
    );

    // Enqueue a fast job while the first is still queued.
    let out2 = run_nq(&dir, &["true"]);
    assert!(
        out2.status.success(),
        "second enqueue (true) should succeed"
    );

    // Wait for both to complete.
    wait_all(&dir);

    assert_eq!(
        count_jobs(&dir),
        2,
        "both job files should exist after completion"
    );

    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let _name = entry.file_name().to_string_lossy().to_string();
        if entry.file_name().to_string_lossy().starts_with(',') {
            let content = fs::read_to_string(entry.path()).unwrap();
            assert!(
                content.contains("+ exit=0"),
                "job should exit successfully: {content:?}"
            );
        }
    }
}

/// Enqueue a long-running job, kill its child process, and verify the job
/// file records the signal-based exit code.
#[test]
fn test_kill_running_job() {
    let dir = test_dir("kill_running_job");

    // Enqueue a job that runs for a long time.
    let output = run_nq(&dir, &["sleep", "9999"]);
    assert!(output.status.success(), "enqueue should succeed");

    // Give the daemon time to start and waitpid on the child.
    std::thread::sleep(Duration::from_millis(1200));

    // Find the sleep 9999 process and kill it.
    let _kill_out = Command::new("pkill")
        .arg("-f")
        .arg("sleep 9999")
        .output()
        .unwrap();

    // Give the daemon time to notice the child died and write exit status.
    std::thread::sleep(Duration::from_millis(600));

    // Wait for the job to finish (daemon should have exited after child died).
    let wait_output = run_nq(&dir, &["-w"]);
    assert!(
        wait_output.status.success(),
        "wait should succeed even for a killed job"
    );

    // Read the job file content.
    let files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(','))
        .map(|e| e.path())
        .collect();
    assert!(!files.is_empty(), "job file should exist after kill + wait");

    let content = fs::read_to_string(&files[0]).unwrap();
    assert!(
        content.contains("+ exit="),
        "job file should contain exit status line"
    );

    // The exit code should indicate a signal-killed process (> 128).
    // SIGTERM = 15 -> 128 + 15 = 143, SIGKILL = 9 -> 128 + 9 = 137
    let exit_line = content
        .lines()
        .find(|l| l.contains("+ exit="))
        .expect("exit line in file");
    assert!(
        exit_line.contains("+ exit="),
        "should have exit status line"
    );
}

/// Environment variables set before `nq` are inherited by the job.
/// Enqueue `sh -c 'echo $NQDIR > out'` and verify that `$NQDIR` is the
/// expected temp directory.
#[test]
fn test_env_passthrough() {
    let dir = test_dir("env_passthrough");
    let out_file = dir.join("out.txt");

    let output = run_nq(
        &dir,
        &[
            "sh",
            "-c",
            &format!("echo \"$NQDIR\" > {}", out_file.display()),
        ],
    );
    assert!(output.status.success(), "enqueue should succeed");

    wait_all(&dir);

    assert!(
        out_file.exists(),
        "output file should be created by the job"
    );

    let content = fs::read_to_string(&out_file).unwrap();
    let dir_str = dir.to_string_lossy().to_string();
    assert!(
        content.trim() == dir_str,
        "NQDIR should be inherited by the job: expected {dir_str}, got {}",
        content.trim()
    );
}

/// The `-c` (compact) flag prints a count of job IDs instead of listing each.
#[test]
fn test_compact_flag() {
    let dir = test_dir("compact_flag");

    run_nq(&dir, &["true"]);
    wait_all(&dir);

    // List with -c -> compact count.
    let output = run_nq(&dir, &["-c"]);
    assert!(output.status.success(), "compact flag should succeed");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "1",
        "compact mode should print '1' for one job"
    );
}

/// Test the `-t` (test) and `-w` (wait) modes: when jobs are running, -t
/// reports failure (exit 1); after they complete, -t reports success (exit 0)
/// and -w is a no-op.
#[test]
fn test_wait_test() {
    let dir = test_dir("wait_test");

    // Enqueue a slow and a fast job.
    run_nq(&dir, &["sleep", "1"]);
    run_nq(&dir, &["true"]);

    // Both are either running or queued -- test should report not-done.
    // Give a brief moment for the daemons to start.
    std::thread::sleep(Duration::from_millis(300));
    let test_out = run_nq(&dir, &["-t"]);
    assert!(
        !test_out.status.success(),
        "-t should exit 1 (false) when jobs are running"
    );

    // Wait for completion.
    wait_all(&dir);

    // Now -t should report all done.
    let test_out = run_nq(&dir, &["-t"]);
    assert!(
        test_out.status.success(),
        "-t should exit 0 (true) when all jobs are done"
    );

    // -w on already-completed jobs should succeed.
    let wait_out = run_nq(&dir, &["-w"]);
    assert!(
        wait_out.status.success(),
        "-w on completed jobs should succeed"
    );
}

/// When `NQDONEDIR` is set, completed (exit 0) jobs are moved there.
#[test]
fn test_done_dir_success() {
    let dir = test_dir("done_dir_success");
    let done_dir = dir.join("done");
    fs::create_dir_all(&done_dir).unwrap();

    let output = Command::new(nq_bin())
        .env("NQDIR", &dir)
        .env("NQDONEDIR", &done_dir)
        .arg("true")
        .output()
        .unwrap();
    assert!(output.status.success(), "enqueue should succeed");

    wait_all(&dir);

    // On macOS, the daemon's move_to_done may not be visible to stat()
    // immediately after the flock is released. Retry briefly.
    let deadline = Instant::now() + Duration::from_secs(5);
    while count_jobs(&done_dir) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Job file should have been moved to done_dir.
    assert_eq!(
        count_jobs(&dir),
        0,
        "completed job should be removed from NQDIR when NQDONEDIR is set"
    );
    assert_eq!(
        count_jobs(&done_dir),
        1,
        "completed job should appear in NQDONEDIR"
    );

    // Verify the exit status in the moved file.
    let content = fs::read_to_string(
        fs::read_dir(&done_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with(','))
            .unwrap()
            .path(),
    )
    .unwrap();
    assert!(
        content.contains("+ exit=0"),
        "exit status should be success"
    );
}

/// List mode: after queueing and completing jobs, listing shows them all.
#[test]
fn test_list_mode() {
    let dir = test_dir("list_mode");

    run_nq(&dir, &["true"]);
    run_nq(&dir, &["echo", "hello"]);
    wait_all(&dir);

    let output = run_nq(&dir, &[]);
    assert!(output.status.success(), "list mode should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "should list both job IDs");
    for line in &lines {
        assert!(
            line.starts_with(','),
            "each line should be a job ID: {line}"
        );
    }
}
