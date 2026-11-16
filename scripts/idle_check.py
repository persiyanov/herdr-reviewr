#!/usr/bin/env python3
"""Idle check: a real reviewr binary in a PTY must sit still while nothing happens.

Starts the binary on a fresh repo with the event log on, beside a fake herdr that shows the pane,
lets the first load settle, then counts what the loop does for a quiet window: every wake and every
process start it logs. Then it edits a file and expects the watcher to report it, a refresh to start
git, and the file's name painted, within two seconds: the counters see work when there is some.
Then herdr hides the pane, and more edits must start nothing at all, until it is shown again and
catches up at once. Then it types two keys in one write and expects the second (`q`) to quit at
once, so a key crossterm already buffered never waits behind a sleep. Exits non-zero on any idle wake, any process start while idle or
hidden, idle CPU past its bound, a missed edit, or a stranded key. Run after a release build:

    python3 scripts/idle_check.py --binary target/release/herdr-reviewr
"""

import argparse
import fcntl
import json
import os
import pty
import select
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


class FakeHerdr:
    """herdr 0.9.3's socket, as reviewr reads it: one request line per connection, and a
    subscription that streams events. The pane `w1:p1` sits in tab `w1:t1`."""

    def __init__(self, path):
        self.focused = "w1:t1"
        self.subscribers = []
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(path)
        self.server.listen()
        threading.Thread(target=self.serve, daemon=True).start()

    def snapshot(self):
        pane = {"pane_id": "w1:p1", "terminal_id": "term", "tab_id": "w1:t1", "workspace_id": "w1"}
        return {"focused_tab_id": self.focused, "panes": [pane], "agents": []}

    def serve(self):
        while True:
            conn, _ = self.server.accept()
            request = json.loads(conn.makefile().readline())
            method, rid = request["method"], request["id"]
            if method == "ping":
                result = {"type": "pong", "version": "0.9.3"}
            elif method == "session.snapshot":
                result = {"type": "session_snapshot", "snapshot": self.snapshot()}
            else:
                result = {"type": "subscription_started"}
            conn.sendall((json.dumps({"id": rid, "result": result}) + "\n").encode())
            if method == "events.subscribe":
                self.subscribers.append(conn)
            else:
                conn.close()

    def focus(self, tab):
        self.focused = tab
        line = (json.dumps({"event": "tab.focused", "data": {}}) + "\n").encode()
        for conn in self.subscribers:
            try:
                conn.sendall(line)
            except OSError:
                pass


def git(repo, *args):
    subprocess.run(["git", "-C", repo, *args], check=True, capture_output=True)


def fixture(root):
    repo = os.path.join(root, "repo")
    os.makedirs(repo)
    git(repo, "init", "-q", "-b", "main")
    for i in range(50):
        with open(os.path.join(repo, f"f{i}.txt"), "w") as f:
            f.write(f"line {i}\n")
    git(repo, "add", "-A")
    git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init")
    with open(os.path.join(repo, "f1.txt"), "a") as f:
        f.write("edited\n")
    # A second worktree to switch to, with a change only it has.
    other = os.path.join(root, "wt-switched")
    git(repo, "worktree", "add", "-q", "-b", "switched", other)
    with open(os.path.join(other, "only-there.txt"), "w") as f:
        f.write("new\n")
    return repo


def cpu_seconds(pid):
    """The CPU time `pid` has used, all its threads together."""
    if os.path.exists(f"/proc/{pid}/stat"):
        fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    # macOS `ps` spells it `M:SS.ss`.
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True)
    minutes, seconds = out.stdout.strip().split(":")[-2:]
    return int(minutes) * 60 + float(seconds)


def pump(fd, seconds):
    """Read the pane's output for `seconds`, so the binary never blocks on a full PTY; returns it."""
    out = b""
    end = time.monotonic() + seconds
    while (left := end - time.monotonic()) > 0:
        ready, _, _ = select.select([fd], [], [], left)
        if ready:
            try:
                out += os.read(fd, 65536)
            except OSError:
                return out
    return out


def quiet_window(fd, pid, log, seconds):
    """Leave the pane alone for `seconds`: the log lines it wrote meanwhile and the CPU it used."""
    start = os.path.getsize(log) if os.path.exists(log) else 0
    cpu_before = cpu_seconds(pid)
    pump(fd, seconds)
    cpu = cpu_seconds(pid) - cpu_before
    with open(log) as f:
        f.seek(start)
        return [line.rstrip() for line in f], cpu


def report_still(label, lines, cpu):
    """Print a still phase's wakes and process starts; whether the pane stirred."""
    stirred = [line for line in lines if " wake " in line or " spawn " in line]
    print(f"{label}: {len(stirred)} wakes or process starts, cpu {cpu * 1000:.0f}ms")
    # A thread spinning on its own (a watcher feeding itself) wakes no loop but burns CPU.
    busy = cpu > 0.02
    if busy:
        print(f"{label}: used CPU with nothing happening")
    for line in stirred[:10]:
        print("  " + line)
    return bool(stirred) or busy


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--settle", type=float, default=5.0, help="covers the 2s keyboard-protocol probe a bare PTY never answers")
    parser.add_argument("--quiet", type=float, default=10.0)
    parser.add_argument("--hidden", type=float, default=3.0)
    parser.add_argument("--search", type=float, default=5.0)
    args = parser.parse_args()
    binary = os.path.abspath(args.binary)

    with tempfile.TemporaryDirectory() as root:
        repo = fixture(root)
        log = os.path.join(root, "events.log")
        config_dir = os.path.join(root, "config")
        os.makedirs(config_dir)
        herdr_socket = os.path.join(root, "herdr.sock")
        herdr = FakeHerdr(herdr_socket)
        env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
        env.update(
            TERM="xterm-256color",
            HERDR_REVIEW_LOG=log,
            HERDR_PLUGIN_CONFIG_DIR=config_dir,
            HERDR_BIN_PATH="false",
            HERDR_SOCKET_PATH=herdr_socket,
            HERDR_PANE_ID="w1:p1",
            HERDR_WORKSPACE_ID="w1",
        )
        pid, fd = pty.fork()
        if pid == 0:
            # A sized terminal, so frames paint and an edit can be seen on screen.
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            os.chdir(repo)
            os.execve(binary, [binary, repo], env)
        try:
            pump(fd, args.settle)
            quiet, quiet_cpu = quiet_window(fd, pid, log, args.quiet)
            # An edit must reach the pane through the watcher: nothing else would notice it.
            edit_at = os.path.getsize(log)
            with open(os.path.join(repo, "f2.txt"), "a") as f:
                f.write("edited by the check\n")
            seen_in = refreshed = painted = None
            screen = b""
            edited = time.monotonic()
            while time.monotonic() - edited < 2 and (refreshed is None or painted is None):
                screen += pump(fd, 0.05)
                if painted is None and b"f2.txt" in screen:
                    painted = time.monotonic() - edited
                with open(log) as f:
                    f.seek(edit_at)
                    after = f.read()
                if seen_in is None and " watch paths=" in after:
                    seen_in = time.monotonic() - edited
                if " refresh " in after and " spawn " in after:
                    refreshed = time.monotonic() - edited
            # Hidden, the pane starts nothing, whatever the worktree does: the file it already
            # knows is edited again, and again.
            herdr.focus("w1:t9")
            pump(fd, 0.5)
            hidden_at = os.path.getsize(log)
            cpu_before = cpu_seconds(pid)
            # A window resize reaches the hidden pane too: it is the terminal, not the reviewer.
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 41, 121, 0, 0))
            end = time.monotonic() + args.hidden
            while time.monotonic() < end:
                with open(os.path.join(repo, "f2.txt"), "a") as f:
                    f.write("edited while hidden\n")
                pump(fd, 0.2)
            hidden_cpu = cpu_seconds(pid) - cpu_before
            with open(log) as f:
                f.seek(hidden_at)
                hidden = [line.rstrip() for line in f]
            shown_at = os.path.getsize(log)
            herdr.focus("w1:t1")
            pump(fd, 1.0)
            with open(log) as f:
                f.seek(shown_at)
                caught_up = any(" refresh " in line for line in f)
            # The search screen open on a settled query sits as still as the diff view.
            os.write(fd, b"/line")
            pump(fd, 3.0)
            searching, search_cpu = quiet_window(fd, pid, log, args.search)
            os.write(fd, b"\x1b")
            pump(fd, 0.5)
            # `W`, a filter and enter restart the pane on the other worktree, with no shell between.
            os.write(fd, b"Wswitched")
            pump(fd, 0.5)
            os.write(fd, b"\r")
            switch_screen = b""
            switched_in = None
            pressed = time.monotonic()
            while time.monotonic() - pressed < 3 and switched_in is None:
                switch_screen += pump(fd, 0.05)
                if b"only-there.txt" in switch_screen:
                    switched_in = time.monotonic() - pressed
            left_screen = b"\x1b[?1049l" in switch_screen
            pump(fd, args.settle)
            switched_quiet, switched_cpu = quiet_window(fd, pid, log, args.quiet)
            # `1` (a no-op tab press) and `q` arrive together; crossterm parses both from one read.
            os.write(fd, b"1q")
            quit_in = None
            typed = time.monotonic()
            while time.monotonic() - typed < 3:
                pump(fd, 0.05)
                if os.waitpid(pid, os.WNOHANG)[0] == pid:
                    quit_in = time.monotonic() - typed
                    pid = None
                    break
        finally:
            if pid is not None:
                os.kill(pid, 9)
                os.waitpid(pid, 0)
            log_text = open(log).read() if os.path.exists(log) else ""

    idle_stirred = report_still(f"quiet {args.quiet:.0f}s", quiet, quiet_cpu)
    search_stirred = report_still(f"search open {args.search:.0f}s", searching, search_cpu)
    hidden_spawns = [line for line in hidden if " spawn " in line]
    resize_shown = any("visible=true" in line for line in hidden)
    if resize_shown:
        print("a window resize counted as the reviewer showing the hidden pane")
    was_hidden = any("visible=false" in line for line in log_text.splitlines())
    print(f"hidden {args.hidden:.0f}s of edits: {len(hidden_spawns)} process starts, cpu {hidden_cpu * 1000:.0f}ms")
    # Each edit still wakes the watcher's thread; that work stays a few milliseconds.
    hidden_busy = hidden_cpu > 0.1
    if hidden_busy:
        print("a hidden pane used CPU past its bound")
    for line in hidden_spawns[:10]:
        print("  " + line)
    if not was_hidden:
        print("the pane never saw itself hidden")
    if seen_in is None:
        print("an edit never reached the watcher")
        for line in log_text.splitlines():
            if " watch " in line:
                print("  " + line)
    else:
        print(f"an edit reached the watcher in {seen_in * 1000:.0f}ms")
    if refreshed is None:
        print("an edit never started a refresh's git: the counters may be blind")
    else:
        print(f"an edit started a refresh's git in {refreshed * 1000:.0f}ms")
    if painted is None:
        print("an edited file never appeared on screen")
    else:
        print(f"an edited file appeared on screen in {painted * 1000:.0f}ms")
    print("shown again, it caught up at once" if caught_up else "shown again, it never refreshed")
    if switched_in is None:
        print("a switch never painted the other worktree's change")
    else:
        print(f"a switch painted the other worktree in {switched_in * 1000:.0f}ms")
    if left_screen:
        print("a switch left the alternate screen: the shell flashed between loops")
    switched_stirred = report_still(f"switched, quiet {args.quiet:.0f}s", switched_quiet, switched_cpu)
    if quit_in is None:
        print("the second key of `1q` never quit: a buffered key waited behind a sleep")
    else:
        print(f"`1q` in one write quit in {quit_in * 1000:.0f}ms")
    failed = (
        idle_stirred
        or search_stirred
        or hidden_spawns
        or hidden_busy
        or resize_shown
        or not was_hidden
        or seen_in is None
        or refreshed is None
        or painted is None
        or not caught_up
        or switched_in is None
        or left_screen
        or switched_stirred
        or quit_in is None
    )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
