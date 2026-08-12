//! Small helpers for locating external command-line tools.

use std::process::Child;
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rustix::process::{Pid as RustixPid, WaitId, WaitIdOptions, waitid};

/// Whether `name` resolves to an executable on `PATH` — a dependency-free `which`. Both shipped
/// platforms are unix, so a file in a `PATH` directory is the executable. Shared by the clipboard
/// probe (`export.rs`) and the URL-opener probe (`browser.rs`).
#[must_use]
pub fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// Wait for a child within the renderer invocation's remaining wall-clock budget, then terminate
/// its process group and reap it. The caller shares one deadline across stdin delivery, stdout
/// capture, and process exit so no phase receives a fresh timeout (`specs/markdown.md`).
pub(crate) fn wait_bounded(child: &mut Child, grace: Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + grace;
    let pid = RustixPid::from_child(child);
    loop {
        let flags = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        match waitid(WaitId::Pid(pid), flags) {
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                terminate_process_group(child);
                return None;
            }
            Ok(Some(_)) => {
                // The leader remains waitable, so its PID still identifies this process group.
                // Kill any detached-stdio descendants before reaping the leader.
                let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
                return child.wait().ok();
            }
        }
    }
}

/// Terminate the renderer's whole process group, then kill and reap its direct child. Renderer
/// commands run in a fresh group so a timeout cannot leave descendants holding the I/O pipes.
pub(crate) fn terminate_process_group(child: &mut Child) {
    let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}
