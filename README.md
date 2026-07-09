# nq-rs

[![CI](https://github.com/hermes98761234/nq-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/hermes98761234/nq-rs/actions/workflows/ci.yml)

A lightweight job queue for Unix and Windows, written in Rust. Based on the
original [`nq`](https://github.com/leahneukirchen/nq) by Leah Neukirchen.

## Overview

`nq` lets you queue shell commands and run them one at a time — no daemon, no
server, no configuration. Jobs are ordered by a filesystem directory, and
ordering is enforced via **flock(2)** file locks. If the system crashes or is
rebooted, your queue survives intact.

**Three tools are provided:**

| Tool       | Description                                                        |
|------------|--------------------------------------------------------------------|
| `nq`       | Queue jobs, list the queue, wait for jobs, or test completion      |
| `nqtail`   | Follow the output of a running or queued job (like `tail -f`)      |
| `nqterm`   | Queue a job and open a tmux/screen window to watch its output      |

## Installation

### Pre-built binaries (Linux, macOS, Windows)

Download the latest release for your platform from the
[Releases page](https://github.com/hermes98761234/nq-rs/releases):

- `nq-x86_64-unknown-linux-gnu.tar.gz`
- `nq-aarch64-apple-darwin.tar.gz`
- `nq-x86_64-pc-windows-msvc.zip`

Each archive contains all three binaries (`nq`, `nqtail`, `nqterm`). Extract
them and place them somewhere on your `PATH`.

### From source

```sh
git clone https://github.com/hermes98761234/nq-rs.git
cd nq-rs
cargo build --release
```

The binaries will be in `target/release/` (`nq`, `nqtail`, `nqterm`).

## Usage

### Queue a job

```sh
nq ffmpeg -i input.mp4 output.mkv
```

This queues the `ffmpeg` command and prints the job ID to stderr:

```
,14f6f3034f8.17035
```

The job runs in the background as soon as all earlier-queued jobs have
completed. Output and exit status are written back to the job file.

### List the queue

```sh
nq
```

Lists all job IDs in the queue, sorted by submission order (oldest first).
Use `-c` for a compact count:

```sh
nq -c
# 3
```

### Wait for jobs

```sh
nq -w              # wait for all queued jobs
nq -w ,abc.12345   # wait for specific job(s)
```

Blocks until the specified jobs finish.

### Test job completion

```sh
nq -t              # exit 0 if all jobs done, exit 1 otherwise
nq -t ,abc.12345   # exit 0 if specific job(s) done
```

Useful in shell scripts:

```sh
while ! nq -t; do sleep 1; done
```

### Quiet enqueue

```sh
nq -q make
```

Suppresses the job ID output on stderr.

### Follow output (nqtail)

Watch a job's output as it runs (like `tail -f`):

```sh
nqtail ,abc.12345
```

Other flags:

| Flag           | Description                                                         |
|----------------|---------------------------------------------------------------------|
| `-a`           | Follow all jobs in the queue (default when no IDs are given)        |
| `-n`           | No wait — if the job is already done, print output and exit         |
| `-q`           | Quiet — print one line per job ID and exit, don't follow output     |

### Terminal multiplexer integration (nqterm)

If you use **tmux** or **screen**, `nqterm` queues a command and immediately
opens a new window that follows its output:

```sh
nqterm cargo build --release
```

This runs `nq cargo build --release`, captures the job ID, and spawns a new
window running `nqtail JOBID`. The window stays open after the job completes
so you can inspect the output.

`nqterm` auto-detects whether you are inside `tmux` (`$TMUX`) or `screen`
(`$STY`). It exits with an error if neither is found.

### Environment variables

| Variable      | Default | Description                                                         |
|---------------|---------|---------------------------------------------------------------------|
| `NQDIR`       | `.`     | Directory where job files are stored (spool directory)              |
| `NQDONEDIR`   | —       | Optional directory for completed (exit 0) jobs — jobs moved here    |
| `NQFAILDIR`   | —       | Optional directory for failed (exit ≠ 0) jobs — jobs moved here     |
| `NQJOBID`     | —       | *(reserved)* Job ID of the currently running job                    |

Example — keeping completed and failed jobs in separate directories:

```sh
export NQDIR=/var/spool/nq
export NQDONEDIR=/var/spool/nq/done
export NQFAILDIR=/var/spool/nq/fail
nq make
```

### Job files

Job files live in `$NQDIR` and are named `,TIMESTAMP.PID`, where the
timestamp is in milliseconds since Unix epoch (hex) and the PID identifies
the creating process. Each job file contains:

1. An `exec` line with the command to run (properly shell-escaped)
2. The stdout and stderr of the command (appended as it runs)
3. An `+ exit=N` line at the end

The files are ordered by name (chronological), and the `flock` system call
ensures that only one job runs at a time.

## How it works

When you run `nq CMD...`:

1. A job file is created in `$NQDIR` with an exclusive `flock` lock.
2. The exec line is written and the lock is held.
3. A daemon process is spawned that inherits the lock.
4. The daemon acquires the exclusive lock, then waits for all older job
   files to be unlocked (meaning they have finished).
5. The daemon runs the command, redirecting stdout/stderr to the job file.
6. When the command finishes, the exit code is written and the job file is
   optionally moved to `NQDONEDIR` or `NQFAILDIR`.

No background daemon or database is required — the filesystem and flock
do all the work.

## License

This project is licensed under **CC0 1.0 Universal** — see the
[LICENSE](LICENSE) file for details.
