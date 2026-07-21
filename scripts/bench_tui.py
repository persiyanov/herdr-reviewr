#!/usr/bin/env python3
"""Perceived-latency benchmark for herdr-reviewr.

Drives the real binary through a PTY and measures keypress -> first response
byte (the UI-thread stall the user feels) and keypress -> output quiescence
(frame fully painted). Polling is set far out so every frame is a response to
an injected key, never a poll tick.

Usage:
  scripts/bench_tui.py --binary target/release/herdr-reviewr --fixture
  scripts/bench_tui.py --binary target/release/herdr-reviewr --repo /path/to/repo --label landing
  scripts/bench_tui.py ... --json out.json      # machine-readable results
  scripts/bench_tui.py ... --iterations 12

The fixture repo is generated deterministically (same content every time) in
--fixture-dir (default: <scratch>/reviewr-bench-fixture), so numbers are
comparable across sessions and machines of the same class.
"""

import argparse
import fcntl
import hashlib
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time

COLS, ROWS = 160, 45
# Output gap that ends a frame burst. Must exceed the largest deferred-reload gap: a
# paint-then-refresh switch draws instantly and reloads behind the frame, so a smaller
# window would cut the measurement off before the refreshed frame arrives (and leak
# that late frame into the next timed press).
QUIET_MS = 300
SETTLE_TIMEOUT = 30.0

# --- fixture -----------------------------------------------------------------


def w(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def det_text(seed, lines):
    """Deterministic pseudo-source: content depends only on the seed."""
    out = []
    for i in range(lines):
        h = hashlib.sha256(f"{seed}:{i}".encode()).hexdigest()
        out.append(f"fn f_{h[:8]}() -> u32 {{ 0x{h[8:16]} }} // {h[16:40]}")
    return "\n".join(out) + "\n"


def build_fixture(root):
    """A pinned synthetic repo: 1500 tracked files, a 500KB highlightable file,
    a 12k-file ignored tree, and an uncommitted changeset of 12 files."""
    marker = os.path.join(root, ".fixture-v1")
    if os.path.exists(marker):
        return
    if os.path.exists(root):
        shutil.rmtree(root)
    os.makedirs(root)
    for d in range(50):
        for f in range(30):
            w(os.path.join(root, f"src/mod_{d:02}/file_{f:02}.rs"), det_text(f"{d}/{f}", 40))
    big = det_text("big", 9000)  # ~500KB of .rs, exercises syntect
    w(os.path.join(root, "src/generated.rs"), big)
    w(os.path.join(root, ".gitignore"), "node_modules/\n")
    for d in range(120):
        for f in range(100):
            w(os.path.join(root, f"node_modules/pkg_{d:03}/f_{f:02}.js"), f"// {d}:{f}\n")
    env = {**os.environ, "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z"}
    run = lambda *a: subprocess.run(a, cwd=root, env=env, check=True, capture_output=True)
    run("git", "init", "-q", "-b", "main")
    run("git", "-c", "user.email=b@b", "-c", "user.name=b", "add", "-A")
    run("git", "-c", "user.email=b@b", "-c", "user.name=b", "commit", "-qm", "fixture")
    for d in range(12):  # uncommitted changeset for the Changes tab
        p = os.path.join(root, f"src/mod_{d:02}/file_00.rs")
        with open(p, "a") as f:
            f.write(det_text(f"edit:{d}", 5))
    w(os.path.join(root, "src/untracked_note.rs"), det_text("untracked", 20))
    w(marker, "v1\n")


# --- PTY session -------------------------------------------------------------


class Session:
    def __init__(self, binary, repo):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        env = {**os.environ, "TERM": "xterm-256color"}
        env.pop("HERDR_PLUGIN_CONFIG_DIR", None)  # standalone mode: no plugin config reads
        self.proc = subprocess.Popen(
            [binary, repo, "--poll", "600000"],
            stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True,
        )
        os.close(slave)

    def _read_until_quiet(self, quiet_ms, timeout):
        """Drain output until a quiet gap; returns (first_byte_at, last_byte_at, n_bytes)."""
        first = last = None
        total = 0
        deadline = time.perf_counter() + timeout
        while True:
            now = time.perf_counter()
            if now > deadline:
                break
            wait = quiet_ms / 1000 if last is not None else deadline - now
            r, _, _ = select.select([self.master], [], [], wait)
            if not r:
                if last is not None:
                    break  # quiet gap after data: frame done
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError:
                break
            if not data:
                break
            t = time.perf_counter()
            first = first if first is not None else t
            last = t
            total += len(data)
        return first, last, total

    def settle(self):
        self._read_until_quiet(500, SETTLE_TIMEOUT)

    def press(self, key, timeout=30.0):
        """Send a key; return (ms to first response byte, ms to painted frame)."""
        t0 = time.perf_counter()
        os.write(self.master, key.encode())
        first, last, total = self._read_until_quiet(QUIET_MS, timeout)
        if first is None:
            return None, None
        return (first - t0) * 1000, (last - t0) * 1000

    def close(self):
        try:
            os.write(self.master, b"q")
            self.proc.wait(timeout=3)
        except Exception:
            self.proc.send_signal(signal.SIGKILL)
        os.close(self.master)


# --- scenarios ---------------------------------------------------------------


def result_row(name, firsts, dones):
    """One scenario result. Iteration 1 is reported per-metric — cold first-byte and cold
    painted — so "cold" has exactly one meaning everywhere."""
    return {
        "scenario": name,
        "cold": {"first_ms": round(firsts[0], 1), "painted_ms": round(dones[0], 1)},
        "first_byte": stats(firsts),
        "painted": stats(dones),
    }


def stats(xs):
    xs = sorted(xs)
    n = len(xs)
    return {
        "n": n,
        "min": round(xs[0], 1),
        "median": round(xs[n // 2], 1),
        "p95": round(xs[min(n - 1, int(n * 0.95))], 1),
        "max": round(xs[-1], 1),
    }


def run_bench(binary, repo, label, iters):
    s = Session(binary, repo)
    s.settle()  # initial frame + startup reload
    results = []

    def scenario(name, key, iters_=None, prep=None):
        """Press `key` `iters_` times, timing each. `prep` keys run untimed first.
        Repeated same-tab presses would no-op (set_tab early-returns), so tab
        scenarios use tab_scenario, which hops away untimed before each press."""
        firsts, dones = [], []
        for k in prep or []:
            s.press(k)
        for _ in range(iters_ or iters):
            f, d = s.press(key)
            if f is None:
                print(f"  !! no response for {name} key {key!r}", file=sys.stderr)
                return
            firsts.append(f)
            dones.append(d)
        results.append(result_row(name, firsts, dones))

    def tab_scenario(name, enter_key, leave_key):
        """Time entering `enter_key`'s tab from the other tab, iters times."""
        firsts, dones = [], []
        for _ in range(iters):
            s.press(leave_key)  # untimed: hop away
            f, d = s.press(enter_key)
            if f is None:
                print(f"  !! no response for {name}", file=sys.stderr)
                return
            firsts.append(f)
            dones.append(d)
        results.append(result_row(name, firsts, dones))

    def chained_scenario(name, leave_key, enter_key, chase_key):
        """Time `chase_key` written in the same burst as a tab switch. Catches
        work that a paint-then-refresh switch defers to just after its frame:
        the chased key stalls behind it, and this is where that shows up."""
        firsts, dones = [], []
        for _ in range(iters):
            s.press(leave_key)  # untimed: hop away
            first_ms, last_ms = s.press(enter_key + chase_key)
            if first_ms is None:
                print(f"  !! no response for {name}", file=sys.stderr)
                return
            firsts.append(first_ms)
            dones.append(last_ms)
        results.append(result_row(name, firsts, dones))

    tab_scenario("tab_enter_all_files", "2", "1")
    tab_scenario("tab_enter_changes", "1", "2")
    # The chased key's *painted* time is the acceptance metric: it includes any
    # reload a paint-first switch pushed past its own frame.
    chained_scenario("tab_enter_all_files_then_f", "1", "2", "f")
    scenario("file_next_changes", "f", prep=["1"])
    # All files: expand into the tree first so `f` walks real source files.
    scenario("file_next_all_files", "f", prep=["2", "j", "\x1b[C", "j", "\x1b[C", "j"])
    s.close()
    return {"label": label, "repo": repo, "iterations": iters, "results": results}


def git_rev(cwd):
    try:
        return subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"], cwd=cwd, capture_output=True, text=True
        ).stdout.strip()
    except Exception:
        return "unknown"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--repo", action="append", default=[], help="real repo path (repeatable)")
    ap.add_argument("--label", action="append", default=[], help="label for each --repo")
    ap.add_argument("--fixture", action="store_true", help="include the pinned synthetic repo")
    ap.add_argument("--fixture-dir", default=None)
    ap.add_argument("--iterations", type=int, default=10)
    ap.add_argument("--json", dest="json_out", default=None)
    args = ap.parse_args()

    targets = []
    if args.fixture:
        fd = args.fixture_dir or os.path.join(
            os.environ.get("TMPDIR", "/tmp"), "reviewr-bench-fixture"
        )
        print(f"fixture: {fd}", file=sys.stderr)
        build_fixture(fd)
        targets.append((fd, "fixture"))
    for i, r in enumerate(args.repo):
        targets.append((r, args.label[i] if i < len(args.label) else os.path.basename(r)))
    if not targets:
        ap.error("nothing to benchmark: pass --fixture and/or --repo")

    meta = {
        "binary": args.binary,
        "binary_mtime": int(os.path.getmtime(args.binary)),
        "source_rev": git_rev(os.path.dirname(os.path.abspath(__file__))),
        "cols": COLS, "rows": ROWS, "quiet_ms": QUIET_MS,
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }
    out = {"meta": meta, "runs": []}
    for repo, label in targets:
        print(f"\n== {label} ({repo}) ==")
        run = run_bench(args.binary, repo, label, args.iterations)
        out["runs"].append(run)
        for r in run["results"]:
            fb, p, c = r["first_byte"], r["painted"], r["cold"]
            print(
                f"{r['scenario']:<24} cold {c['first_ms']:>6.1f}/{c['painted_ms']:>7.1f}ms   "
                f"first-byte med {fb['median']:>7.1f}ms p95 {fb['p95']:>7.1f}ms   "
                f"painted med {p['median']:>7.1f}ms"
            )
    if args.json_out:
        with open(args.json_out, "w") as f:
            json.dump(out, f, indent=1)
        print(f"\nwrote {args.json_out}")


if __name__ == "__main__":
    main()
