//! Generalized subprocess runner shared by every forge backend: spawn a CLI tool, drain its
//! pipes while polling a cancellation flag so a large response cannot block the child, and
//! classify how it failed. See `specs/forge-host.md`.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// How a spawned tool run failed, before its stdout/stderr is classified by the caller.
pub(crate) enum RunFail {
    /// The tool binary is not on `PATH`.
    NotFound,
    /// The tool ran and exited non-zero; its stderr, for the caller to classify.
    Failed { stderr: String },
    /// The cancellation flag was observed set while or after the tool ran; the caller
    /// discards this result rather than surfacing it as a failure.
    Cancelled,
    /// Any other I/O failure spawning or waiting on the child.
    Io(String),
}

/// Spawn `tool` with `args` in `repo`, optionally writing `stdin` to the child, draining both
/// pipes while polling `cancelled` so a large response cannot fill a pipe and block the child
/// before it exits. A superseded config/fetch kills the process; the coordinator keeps
/// ownership until this worker reports completion, preserving one real fetch in flight.
pub(crate) fn run_tool(
    tool: &str,
    repo: &Path,
    args: &[&str],
    stdin: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<String, RunFail> {
    run_tool_with_env(tool, repo, args, stdin, &[], cancelled)
}

/// As [`run_tool`], but with `envs` applied to the child's environment on top of the inherited
/// one (each `(key, value)` set via [`Command::env`]). Exists so a caller that needs to change
/// how a specific subprocess behaves — e.g. suppressing `git credential fill`'s terminal
/// prompt — can do so without changing every other `run_tool` call site.
pub(crate) fn run_tool_with_env(
    tool: &str,
    repo: &Path,
    args: &[&str],
    stdin: Option<&str>,
    envs: &[(&str, &str)],
    cancelled: &AtomicBool,
) -> Result<String, RunFail> {
    let mut cmd = Command::new(tool);
    cmd.current_dir(repo).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(RunFail::NotFound),
        Err(e) => return Err(RunFail::Io(e.to_string())),
    };
    if let Some(input) = stdin {
        use std::io::Write;
        let mut pipe = child.stdin.take().expect("piped stdin");
        let _ = pipe.write_all(input.as_bytes());
        drop(pipe); // close so the child sees EOF
    }

    // Drain both pipes while polling so a large response cannot fill a pipe and block
    // the child before it exits. A superseded config/fetch kills the process; the coordinator
    // keeps ownership until this worker reports completion, preserving one real fetch in flight.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(RunFail::Io(error.to_string()));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if cancelled.load(Ordering::Acquire) {
        return Err(RunFail::Cancelled);
    }
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    Err(RunFail::Failed { stderr: String::from_utf8_lossy(&stderr).into_owned() })
}
