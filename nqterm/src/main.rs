//! nqterm — enqueue a job and spawn a tmux/screen window to follow its output.
//!
//! Usage:
//!   nqterm CMD...
//!
//! Runs `nq CMD...` to enqueue the command, captures the job ID, then opens a
//! new tmux or screen window running `nqtail JOBID` to follow its output.
//! If neither tmux nor screen is detected, prints an error and exits.

use std::process::{Command, ExitCode, Stdio};

use anyhow::{bail, Context, Result};
use clap::Parser;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// nqterm — enqueue a job and spawn a tmux/screen window to follow its output
#[derive(Parser, Debug)]
#[command(name = "nqterm", version, about)]
struct Cli {
    /// The command and its arguments to enqueue
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    cmd: Vec<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shell-escape a string for single-quote wrapping.
/// Job IDs like `,18c4f3a7e89.12345` are safe inside single quotes.
fn sh_quote(s: &str) -> String {
    // Replace any single quote inside with '\'' (end quote, literal quote, restart quote).
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nqterm: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    // Run `nq CMD...` and capture its output.
    let output = Command::new("nq")
        .args(&cli.cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to execute nq — is it installed and in PATH?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("nq failed:\n{stderr}");
    }

    // nq prints the job ID on stderr (eprintln!) when queueing.
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    let job_id = stderr_str.trim();
    if job_id.is_empty() {
        bail!("nq did not produce a job ID (try running 'nq CMD...' directly)");
    }

    // Detect which terminal multiplexer we're running under.
    let in_tmux = std::env::var("TMUX").is_ok();
    let in_screen = std::env::var("STY").is_ok();

    // The command to run in the new window: follow the job, then keep the
    // window open (cat >/dev/null blocks until stdin is closed).
    let window_cmd = format!(
        "nqtail {} 2>/dev/null; exit; cat >/dev/null",
        sh_quote(job_id)
    );

    if in_tmux {
        let status = Command::new("tmux")
            .args(["new-window", &window_cmd])
            .status()
            .context("failed to spawn tmux new-window")?;
        if !status.success() {
            bail!("tmux new-window exited with status {}", status);
        }
    } else if in_screen {
        let status = Command::new("screen")
            .arg(&window_cmd)
            .status()
            .context("failed to spawn screen window")?;
        if !status.success() {
            bail!("screen exited with status {}", status);
        }
    } else {
        bail!(
            "neither tmux (TMUX) nor screen (STY) detected\n\
             Run nqterm inside a tmux or screen session"
        );
    }

    // Print the job ID so the user can reference it later.
    println!("{job_id}");

    Ok(())
}
