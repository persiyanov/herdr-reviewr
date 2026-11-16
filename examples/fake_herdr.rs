//! A fake herdr CLI: serves the fixtures in `FAKE_HERDR_DIR` and logs each call to `herdr.log`.

#[path = "../tests/common/fixture.rs"]
mod fixture;

use fixture::fixture;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

/// The pane every successful `plugin pane open` creates.
const OPENED: &str = "w1:p9";

fn main() -> ExitCode {
    let dir = PathBuf::from(std::env::var_os("FAKE_HERDR_DIR").expect("FAKE_HERDR_DIR is set"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let line = args.join(" ");
    let mut log =
        fs::OpenOptions::new().create(true).append(true).open(dir.join("herdr.log")).unwrap();
    // One append write per call, so concurrent calls never interleave a line.
    log.write_all(format!("{line}\n").as_bytes()).unwrap();
    drop(log);

    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["pane", "list", ..] => {
            if fs::remove_file(dir.join("list-hang")).is_ok() {
                thread::sleep(Duration::from_secs(30));
            }
            let opened = if dir.join("opened").exists() {
                format!(r#",{{"pane_id":"{OPENED}"}}"#)
            } else {
                String::new()
            };
            let default = format!(r#"{{"result":{{"panes":[{{"pane_id":"w1:p1"}}{opened}]}}}}"#);
            answer(&read(&dir.join("panes.json")).unwrap_or(default))
        }
        ["pane", "process-info", "--pane", pane] => {
            if let Some(failure) = read(&fixture(&dir, "procfail", pane, ".json")) {
                return fail(&failure);
            }
            answer(
                &read(&fixture(&dir, "procinfo", pane, ".json"))
                    .unwrap_or_else(|| process_info(&dir, pane, &line)),
            )
        }
        ["pane", "close", pane] => {
            if let Some(failure) = read(&fixture(&dir, "closefail", pane, "")) {
                return fail(&failure);
            }
            if *pane == OPENED {
                let _ = fs::remove_file(dir.join("opened"));
            }
            answer(r#"{"result":{}}"#)
        }
        ["plugin", "config-dir", ..] => {
            if dir.join("configdir-hang").exists() {
                thread::sleep(Duration::from_secs(5));
            }
            answer(&dir.display().to_string())
        }
        ["plugin", "list", ..] => match read(&dir.join("pluginsfail")) {
            Some(failure) => fail(&failure),
            None => answer(
                &read(&dir.join("plugins.json"))
                    .unwrap_or_else(|| r#"{"result":{"plugins":[]}}"#.to_owned()),
            ),
        },
        ["plugin", "pane", "open", ..] => {
            if let Some(failure) = read(&dir.join("openfail")) {
                return fail(&failure);
            }
            fs::write(dir.join("opened"), "").unwrap();
            answer(&format!(
                r#"{{"result":{{"type":"plugin_pane_opened","plugin_pane":{{"pane":{{"pane_id":"{OPENED}","tab_id":"w1:t9"}}}}}}}}"#
            ))
        }
        ["agent", "list"] => match read(&dir.join("agentsfail")) {
            Some(failure) => fail(&failure),
            None => answer(
                &read(&dir.join("agents.json"))
                    .unwrap_or_else(|| r#"{"result":{"agents":[]}}"#.to_owned()),
            ),
        },
        ["tab", "list", ..] => answer(
            &read(&dir.join("tabs.json")).unwrap_or_else(|| r#"{"result":{"tabs":[]}}"#.to_owned()),
        ),
        _ => answer(r#"{"result":{}}"#),
    }
}

/// The default process-info answer for `pane`, whose read is the logged `line`.
fn process_info(dir: &Path, pane: &str, line: &str) -> String {
    let process = if pane == OPENED {
        let empty_reads: usize = read(&dir.join("opened-empty-reads"))
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0);
        let reads = read(&dir.join("herdr.log")).unwrap_or_default();
        if reads.lines().filter(|logged| *logged == line).count() <= empty_reads {
            // herdr omits an empty list.
            return format!(
                r#"{{"result":{{"process_info":{{"pane_id":"{pane}","shell_pid":1}}}}}}"#
            );
        }
        r#"{"pid":9,"name":"herdr-reviewr","argv0":"herdr-reviewr","argv":["/plugin/bin/herdr-reviewr"],"cwd":"/w"}"#
    } else {
        r#"{"pid":7,"name":"zsh","argv0":"zsh","argv":["-zsh"],"cwd":"/"}"#
    };
    format!(
        r#"{{"result":{{"process_info":{{"foreground_process_group_id":7,"foreground_processes":[{process}],"pane_id":"{pane}","shell_pid":1}}}}}}"#
    )
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn answer(stdout: &str) -> ExitCode {
    println!("{}", stdout.trim_end());
    ExitCode::SUCCESS
}

fn fail(stderr: &str) -> ExitCode {
    eprintln!("{}", stderr.trim_end());
    ExitCode::FAILURE
}
