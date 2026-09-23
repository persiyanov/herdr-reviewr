#!/usr/bin/env python3
"""Prove `e` really suspends the pane, runs the editor, and comes back.

Unit tests stop at the argv. Everything after it is terminal state: leaving the alternate
screen, releasing raw mode, and rebuilding both on return. Only a real PTY shows that, so this
drives the real binary with a scripted editor and checks the bytes on the wire.

    python3 scripts/smoke_edit_file.py --binary target/release/herdr-reviewr

Exits 0 when every check passes, 1 with the failing check named otherwise.
"""

import argparse
import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

ROWS, COLS = 40, 120
# Every session reads a config dir. Left unset, reviewr falls back to the real installed one
# and the suite would run against whatever the machine's own `editor` key says — opening the
# reviewer's actual editor. `main` points this at an empty directory before any session starts.
NO_CONFIG = None
ALT_ENTER = b"\x1b[?1049h"
ALT_LEAVE = b"\x1b[?1049l"
BRACKETED_PASTE_ON = b"\x1b[?2004h"
MOUSE_SGR_ON = b"\x1b[?1006h"
EDITED_LINE = b"EDITED-BY-THE-SCRIPTED-EDITOR"
CSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]")


def plain(data):
    """The bytes with the escape codes taken out.

    A frame paints only the cells that changed, so a word can arrive split across cursor
    moves and colour changes. Text checks read this, never the raw stream.
    """
    return CSI.sub(b"", data)


def sh(cwd, *args):
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def make_repo(root):
    """A repo with one committed file and one uncommitted edit, so `Changes` has a diff."""
    sh(root, "git", "init", "-q")
    sh(root, "git", "config", "user.email", "smoke@example.com")
    sh(root, "git", "config", "user.name", "Smoke")
    path = os.path.join(root, "a.rs")
    with open(path, "w") as f:
        f.write("one\ntwo\nthree\nfour\n")
    sh(root, "git", "add", "-A")
    sh(root, "git", "commit", "-qm", "init")
    with open(path, "w") as f:
        f.write("one\nTWO\nthree\nfour\nfive\n")
    # The write lands in the same second as the commit, so git's stat cache can still call the
    # file clean. Correct it here, or the pane starts on an empty changeset.
    subprocess.run(["git", "update-index", "--refresh"], cwd=root, capture_output=True)
    return path


def make_editor(bindir, argv_log, name, holds=0):
    """A scripted `$EDITOR` under a real editor's name, so its dialect resolves.

    Records its argv, writes to the file, and prints to the terminal. It lives outside the
    repository, or reviewr would list it as an untracked change and open it instead. `holds`
    seconds stand in for the time a reviewer spends with the file open.
    """
    os.makedirs(bindir, exist_ok=True)
    script = os.path.join(bindir, name)
    with open(script, "w") as f:
        f.write(
            "#!/bin/sh\n"
            f'printf "%s\\n" "$@" > {argv_log}\n'
            f'printf "cwd=%s\\npath=%s\\n" "$PWD" "$PATH" > {argv_log}.env\n'
            # Proof the editor owns a real terminal: this reaches the screen only if reviewr
            # actually left the alternate screen.
            'printf "FAKE-EDITOR-IS-ON-SCREEN\\n"\n'
            # The last argument carries the path in every dialect, bare or `path:line`.
            'for a in "$@"; do last="$a"; done\n'
            'printf "%s\\n" "${last%%:*}" > /dev/null\n'
            f'printf "{EDITED_LINE.decode()}\\n" >> "${{last%%:*}}"\n'
            f"sleep {holds}\n"
            "exit 0\n"
        )
    os.chmod(script, 0o755)
    return script


class Session:
    def __init__(self, binary, repo, editor, visual=None, config_dir=None, poll=600000):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        env = {**os.environ, "TERM": "xterm-256color"}
        env.pop("VISUAL", None)
        env.pop("EDITOR", None)
        if editor:
            env["EDITOR"] = editor
        if visual:
            env["VISUAL"] = visual
        # Never the machine's own: an empty directory is the missing-file default.
        env["HERDR_PLUGIN_CONFIG_DIR"] = config_dir or NO_CONFIG
        self.proc = subprocess.Popen(
            [binary, repo, "--poll", str(poll)],
            stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True,
        )
        os.close(slave)
        self.seen = b""

    def drain(self, quiet=0.5, timeout=20.0):
        deadline = time.perf_counter() + timeout
        got = b""
        while time.perf_counter() < deadline:
            r, _, _ = select.select([self.master], [], [], quiet if got else 1.0)
            if not r:
                if got:
                    break
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            got += data
        self.seen += got
        return got

    def press(self, key):
        os.write(self.master, key.encode())
        return self.drain()

    def press_bounded(self, key, seconds):
        """Send a key and read for a fixed span.

        A fixed window, not [`drain`]'s quiet one, so a check can look at the screen at a chosen
        moment during an edit rather than whenever the pane happens to fall silent.
        """
        os.write(self.master, key.encode())
        return self.drain(quiet=0.3, timeout=seconds)

    def cpu_seconds(self):
        """Processor time the pane has used so far."""
        out = subprocess.run(
            ["ps", "-o", "time=", "-p", str(self.proc.pid)], capture_output=True, text=True
        ).stdout.strip()
        minutes, seconds = out.split(":")[-2:]
        return int(minutes) * 60 + float(seconds)

    def close(self):
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        os.close(self.master)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/herdr-reviewr")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    if not os.path.exists(binary):
        sys.exit(f"no binary at {binary}: cargo build --release first")

    failures = []

    def check(name, ok, detail=""):
        print(f"{'ok  ' if ok else 'FAIL'}  {name}{'  ' + detail if detail and not ok else ''}")
        if not ok:
            failures.append(name)

    with tempfile.TemporaryDirectory() as home:
        global NO_CONFIG
        NO_CONFIG = os.path.join(home, "no-config")
        os.makedirs(NO_CONFIG)
        root = os.path.join(home, "repo")
        bindir = os.path.join(home, "bin")
        os.makedirs(root)
        file_path = make_repo(root)
        argv_log = os.path.join(home, "argv.txt")
        # It holds the pane for a moment, so a key can be typed while the editor owns it.
        editor = make_editor(bindir, argv_log, "vim", holds=2)

        s = Session(binary, root, editor)
        s.drain()  # startup paint
        check("the pane starts on the alternate screen", ALT_ENTER in s.seen)

        before = len(s.seen)
        os.write(s.master, b"e")
        time.sleep(0.7)  # the editor owns the pane by now
        # Type-ahead, and the editor's own teardown queries answer in the same bytes: reviewr
        # must discard what was buffered rather than read `q` as a command on the way back.
        os.write(s.master, b"q")
        # Read until the pane is back: the editor is still holding it when the quiet window
        # would otherwise close.
        deadline = time.perf_counter() + 20
        while ALT_ENTER not in s.seen[before:] and time.perf_counter() < deadline:
            s.drain(quiet=0.3, timeout=2.0)
        s.drain()
        after = s.seen[before:]
        check("what was typed while the editor held the pane is discarded",
              s.proc.poll() is None, "reviewr acted on the buffered key and exited")

        check("`e` leaves the alternate screen", ALT_LEAVE in after)
        check("the editor's own output reaches the terminal",
              b"FAKE-EDITOR-IS-ON-SCREEN" in plain(after))
        check("the pane re-enters the alternate screen", ALT_ENTER in after)
        check(
            "the alternate screen is left before it is re-entered",
            ALT_LEAVE in after
            and ALT_ENTER in after
            and after.index(ALT_LEAVE) < after.rindex(ALT_ENTER),
        )

        argv = []
        if os.path.exists(argv_log):
            with open(argv_log) as f:
                argv = [line.rstrip("\n") for line in f]
        check("the editor is handed an absolute path", bool(argv) and argv[-1].startswith("/"),
              f"argv={argv}")
        # `vim` resolves to the `+LINE` dialect, so the line arrives in its own form.
        check("and the line under the cursor, in the vi dialect",
              bool(argv) and argv[0].startswith("+") and argv[0][1:].isdigit(),
              f"argv={argv}")

        with open(file_path) as f:
            body = f.read()
        check("the editor's write lands in the worktree", EDITED_LINE.decode() in body)

        # The return refreshes the changeset itself. This session polls every 600s, so nothing
        # ambient can paint the new line: only `run_editor`'s own request puts it on screen.
        check("the edit is on screen without waiting for a poll",
              EDITED_LINE in plain(after),
              "the diff still shows the pre-edit file after the editor returned")

        # Re-claimed with the screen, or mouse selection, gutter drag-commenting and bracketed
        # paste stay dead for the rest of the session after the first `e`.
        check("the input modes are re-claimed with the pane",
              all(mode in after and after.rindex(mode) > after.index(ALT_LEAVE)
                  for mode in (BRACKETED_PASTE_ON, MOUSE_SGR_ON)),
              "an input mode was not re-enabled after the editor returned")

        # The resume must repaint the whole pane, or the editor's leftovers stay on screen.
        check("the resumed frame repaints the whole pane",
              ALT_ENTER in after
              and b"Changes" in after
              and after.rindex(b"Changes") > after.index(ALT_ENTER),
              "the pane did not redraw after re-entering the alternate screen")

        env = {}
        if os.path.exists(argv_log + ".env"):
            with open(argv_log + ".env") as f:
                env = dict(line.rstrip("\n").split("=", 1) for line in f if "=" in line)
        check("the editor runs in the reviewed repository",
              os.path.realpath(env.get("cwd", "")) == os.path.realpath(root),
              f"cwd={env.get('cwd')}")
        check("and inherits the reviewer's own PATH order",
              env.get("path", "").startswith(os.environ.get("PATH", "").split(":")[0]),
              f"path={env.get('path', '')[:80]}")

        check("the status names the edited file", b"edited" in plain(after))

        # `q` is the first key sent after the pane comes back, so this is also what proves the
        # return does not swallow one. Reading output instead would not: the status expires on
        # its own clock and repaints regardless of whether the press was seen.
        s.press("q")
        try:
            quit_code = s.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            quit_code = None
        check("`q` still quits, so the return swallowed no keypress", quit_code == 0,
              f"exit={quit_code}, and None means the pane never saw the key")
        s.close()

        # A window editor takes a different dialect and holds the file the way a reviewer does,
        # so the checks below run against a pane with an editor still open. A real poll, since
        # the poll is what shows the write.
        gui_log = os.path.join(home, "argv-gui.txt")
        gui = make_editor(bindir, gui_log, "code", holds=12)
        # Its own repository, so the earlier session's write is not already on screen.
        gui_root = os.path.join(home, "gui-repo")
        os.makedirs(gui_root)
        make_repo(gui_root)
        s = Session(binary, gui_root, gui, poll=1000)
        s.drain()
        gui_mark = len(s.seen)
        s.press_bounded("e", 2.0)
        gui_argv = []
        if os.path.exists(gui_log):
            with open(gui_log) as f:
                gui_argv = [line.rstrip("\n") for line in f]
        check("a window editor is told no flag reviewr invented",
              not any(a.startswith("--wait") or a.startswith("--block") for a in gui_argv),
              f"argv={gui_argv}")
        check("and takes its line as --goto path:line",
              "-g" in gui_argv and bool(gui_argv) and ":" in gui_argv[-1],
              f"argv={gui_argv}")
        # It never reads the terminal, so reviewr keeps it: the reviewer keeps the diff on
        # screen, and raw mode stays on so a `ctrl+c` in the pane cannot signal the process.
        gui_after = plain(s.seen[gui_mark:])
        check("and the pane is never handed to it", ALT_LEAVE not in s.seen[gui_mark:])
        check("so its own output never reaches the screen",
              b"FAKE-EDITOR-IS-ON-SCREEN" not in gui_after)
        check("the pane says it opened the file", b"opened" in gui_after)
        # `tab` moves focus to the diff, which rewrites the footer's primary action — a change
        # only the key can cause, unlike the ambient repaints this session's 1s poll produces.
        check("the pane answers keys while the editor holds the file",
              b"comment" in plain(s.press_bounded("\t", 1.5)))
        # Nothing is watching the editor, so the pane rests: no wake of its own, no redraw.
        before = s.cpu_seconds()
        s.drain(quiet=0.3, timeout=2.0)
        burned = s.cpu_seconds() - before
        # `ps -o time=` reports hundredths on macOS and whole seconds on Linux, so the bound is
        # exact here and degrades to "did not burn a core" there. Either resolution catches the
        # regression it exists for: a loop asking to wake at a deadline already in the past.
        check("and rests while it waits", burned < 0.2, f"burned {burned:.2f}s of cpu in 2s")
        # Nothing waits for the editor to close: the poll shows the write while the file is
        # still out, with no keypress from the reviewer.
        opened_at = time.perf_counter()
        while EDITED_LINE not in plain(s.seen[gui_mark:]) and time.perf_counter() - opened_at < 8:
            s.drain(quiet=0.3, timeout=1.0)
        check("the poll shows the write with the file still out",
              EDITED_LINE in plain(s.seen[gui_mark:]))
        # A file still out opens again rather than being refused: the reviewer has moved on to
        # another line and wants the editor to follow.
        with open(gui_log, "w") as f:
            f.write("")
        # The status does not change between two `opened` presses, so the argv log is the
        # observable: a refused press would leave it empty.
        s.press_bounded("e", 2.0)
        with open(gui_log) as f:
            again = [line.rstrip("\n") for line in f]
        check("a file already out opens again rather than being refused",
              bool(again) and again[-1].endswith(":1"), f"argv={again}")
        s.press("q")
        s.close()

        # The `editor` config key is the documented override, and nothing else proves it
        # reaches the spawn: the two sources below are both live, and only one may win.
        cfg_dir = os.path.join(home, "cfg")
        os.makedirs(cfg_dir)
        cfg_log = os.path.join(home, "argv-cfg.txt")
        env_log = os.path.join(home, "argv-env.txt")
        configured = make_editor(bindir, cfg_log, "nano")
        from_env = make_editor(bindir, env_log, "micro")
        with open(os.path.join(cfg_dir, "config.toml"), "w") as f:
            f.write(f'editor = "{configured} +{{line}} {{file}}"\n')
        s = Session(binary, root, from_env, config_dir=cfg_dir)
        s.drain()
        s.press("e")
        check("the `editor` config key outranks the environment",
              os.path.exists(cfg_log) and not os.path.exists(env_log))
        s.press("q")
        s.close()

        # `$VISUAL` outranks `$EDITOR`, and the two arrive at the same call as separate
        # arguments, so only a live check catches them being transposed there.
        visual_log = os.path.join(home, "argv-visual.txt")
        os.remove(env_log) if os.path.exists(env_log) else None
        visual = make_editor(bindir, visual_log, "kak")
        s = Session(binary, root, from_env, visual=visual)
        s.drain()
        s.press("e")
        check("$VISUAL outranks $EDITOR",
              os.path.exists(visual_log) and not os.path.exists(env_log))
        s.press("q")
        s.close()

        # A value that survives config validation but names no program is a different cause
        # from an unset editor, and must not be reported as one.
        bare_dir = os.path.join(home, "cfg-bare")
        os.makedirs(bare_dir)
        with open(os.path.join(bare_dir, "config.toml"), "w") as f:
            f.write('editor = "\'\'"\n')
        s = Session(binary, root, None, config_dir=bare_dir)
        s.drain()
        mark = len(s.seen)
        s.press("e")
        check("an `editor` key naming no program says which cause it is",
              b"names no program" in plain(s.seen[mark:]))
        s.press("q")
        s.close()

        # An editor that is not there says so before anything moves. The name has to resolve to
        # a window dialect, or this drives the terminal branch and proves nothing about it.
        s = Session(binary, root, os.path.join(home, "no-such-dir", "code"))
        s.drain()
        mark = len(s.seen)
        s.press("e")
        check("a window editor that is not there says so", b"no editor at" in plain(s.seen[mark:]))
        check("and the pane is never handed over for it", ALT_LEAVE not in s.seen[mark:])
        s.press("q")
        s.close()

        # A file the changeset still names but the worktree no longer holds opens nothing.
        gone_root = os.path.join(home, "gone-repo")
        os.makedirs(gone_root)
        gone = os.path.join(gone_root, "a.rs")
        sh(gone_root, "git", "init", "-q")
        sh(gone_root, "git", "config", "user.email", "smoke@example.com")
        sh(gone_root, "git", "config", "user.name", "Smoke")
        with open(gone, "w") as f:
            f.write("one\ntwo\n")
        sh(gone_root, "git", "add", "-A")
        sh(gone_root, "git", "commit", "-qm", "init")
        os.remove(gone)
        s = Session(binary, gone_root, editor)
        s.drain()
        mark = len(s.seen)
        s.press("e")
        check("a file the worktree no longer holds says it is gone",
              b"is gone" in plain(s.seen[mark:]))
        s.press("q")
        s.close()

        # Every failure path has to reach the reviewer on the frame it happened, not on the
        # next keypress. The loop draws only after an event arrives, so the run has to repaint.
        for label, ed, needle in [
            ("no editor set", None, b"set `editor`"),
            ("a missing editor binary", "/nonexistent/nope", b"no editor at"),
            ("an editor that exits nonzero", "/usr/bin/false", b"editor exited"),
        ]:
            s = Session(binary, root, ed)
            s.drain()
            mark = len(s.seen)
            s.press("e")
            check(f"{label} says so on the press", needle in plain(s.seen[mark:]))
            s.press("q")
            s.close()

    if failures:
        print(f"\n{len(failures)} check(s) failed: {', '.join(failures)}")
        return 1
    print("\nall checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
