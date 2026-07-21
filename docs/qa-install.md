# QA-installing a local build

How to run a locally built `herdr-reviewr` inside the real herdr panes. Follow it exactly. Every
step exists because skipping it has broken a session before.

The installed plugin lives at `~/.config/herdr/plugins/github/persiyanov.reviewr-<hash>/` and its
panes run `bin/herdr-reviewr` from that directory by absolute path. QA means swapping that one
file and restarting the panes.

## The one command

```
just qa-install
```

It builds the release binary, swaps it into the installed plugin, verifies the swap actually
runs, and prints what remains manual. The steps it performs, and why each one is load-bearing:

1. `cargo build --release`.
2. Back up the original binary to `bin/herdr-reviewr.release-backup`, only if no backup exists
   yet. A later run never overwrites the pristine release with an earlier QA build.
3. Replace the binary **through a new inode**: `rm`, `cp` to a staging name, `mv` into place,
   then `codesign --force --sign -` on macOS.
4. Run `bin/herdr-reviewr --resolve-plugin-config` and require exit 0 before touching any pane.
5. Print the pids of running panes still on the old binary.

## Rule 1: never overwrite the binary in place

`cp target/release/herdr-reviewr <plugin>/bin/herdr-reviewr` onto an existing file keeps the old
inode. macOS caches code-signing state by inode, decides the file was tampered with, and
SIGKILLs it at every launch. The symptom: panes open and instantly die, the plugin scripts
report `configuration validation failed`, and the binary exits 137 with no output. Nothing in
the logs says why. Always `rm` first and move a fresh file in.

## Rule 2: replacing the file does not touch running panes

A running pane keeps executing the binary image it was launched with, however many times the
file on disk changes. Refreshing inside reviewr (`r`) or reloading the herdr client does
nothing. Each reviewr pane must be closed and reopened. `pgrep -f herdr-reviewr` with start
times tells you which panes are still old.

## Rule 3: only the user restarts panes

The plugin's `open` and `toggle` actions act on the **focused workspace**, whatever
`HERDR_WORKSPACE_ID` says. Scripting them from outside herdr stacks every new sidebar into
whichever workspace happens to be focused. Closing is safe
(`bash <plugin>/herdr/sidebar.sh close` sweeps only labeled panes in the named workspace), but
opening is not. After the swap, tell the user: press the reviewr toggle in each space you want
on the new build. Do not automate it.

## Verify

- `<plugin>/bin/herdr-reviewr --resolve-plugin-config` exits 0 and prints the config JSON.
- After the user reopens a pane, `ps -o lstart= -p $(pgrep -f herdr-reviewr)` shows a start
  time later than the swap.

## Rollback

```
cd ~/.config/herdr/plugins/github/persiyanov.reviewr-*/bin
cp herdr-reviewr.release-backup herdr-reviewr.staging
rm herdr-reviewr && mv herdr-reviewr.staging herdr-reviewr
```

Then close and reopen the panes, same as any other swap. A full reinstall
(`herdr plugin install persiyanov/herdr-reviewr`) also restores the released binary.
