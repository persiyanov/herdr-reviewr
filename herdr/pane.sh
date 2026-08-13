#!/usr/bin/env bash
# The reviewr pane actions and event hook (specs/herdr-host.md).
#
#   pane.sh toggle      open a reviewr pane, or close every one if any is open
#   pane.sh open        open a reviewr pane, no-op if one is open
#   pane.sh close       close every reviewr pane, no-op if none
#   pane.sh auto-open   worktree.created hook: open, gated by auto_open and placement
#
# A reviewr pane is any pane running the review UI in its foreground process group, read
# live per pane (specs/herdr-host.md, Pane identity). The `reviewr` label is display only
# and never read. There is no state file. Actions refuse loudly (exit 1, one stderr line)
# and report successes on stdout; a refused event reports its config error through stderr
# for herdr's plugin log.
set -uo pipefail

# herdr runs plugin commands with a minimal PATH; ensure jq/git resolve on common installs.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"

mode="${1:-toggle}"
H="${HERDR_BIN_PATH:-herdr}"

# Validate the whole plugin config before reading workspace state or taking any action. The Rust
# binary owns TOML parsing and defaults, so every plugin entry point shares exactly one contract.
if [ -n "${HERDR_REVIEWR_BIN:-}" ]; then
  REVIEWR="$HERDR_REVIEWR_BIN"
elif [ -n "${HERDR_PLUGIN_ROOT:-}" ]; then
  REVIEWR="$HERDR_PLUGIN_ROOT/bin/herdr-reviewr"
else
  REVIEWR="herdr-reviewr"
fi
config_json=$("$REVIEWR" --resolve-plugin-config 2>&1)
config_status=$?
if [ "$config_status" -ne 0 ]; then
  [ -n "$config_json" ] || config_json="reviewr: configuration validation failed"
  printf '%s\n' "$config_json" >&2
  exit 1
fi
# One normalized-config field. The has() form reads a present `false` as a value, where a
# bare `jq -e` would read it as failure — one template serves strings and booleans alike.
cfg_field() {
  printf '%s' "$config_json" | jq -r "if has(\"$1\") then .$1 else error(\"missing $1\") end" 2>/dev/null
}
unreadable_config() {
  printf 'reviewr: normalized configuration is unreadable\n' >&2
  exit 1
}
placement=$(cfg_field toggle_placement) || unreadable_config
direction=$(cfg_field toggle_direction) || unreadable_config
auto_open=$(cfg_field auto_open) || unreadable_config

# The stable launch paths track the live plugin root from here, not from the install step:
# the build step runs in a staging checkout that herdr renames afterwards, so only a runtime
# invocation knows the real root (specs/herdr-host.md, Install paths). Best effort — never
# fails an action, and never replaces anything but a symlink.
if [ -n "${HERDR_PLUGIN_ROOT:-}" ] && [ -x "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" ]; then
  for link_dir in "$HOME/.local/state/herdr/plugins/persiyanov.reviewr/bin" "$HOME/.local/bin"; do
    if [ "$link_dir" = "$HOME/.local/bin" ] && [ ! -d "$link_dir" ]; then
      continue
    fi
    mkdir -p "$link_dir" 2>/dev/null || continue
    if [ -L "$link_dir/herdr-reviewr" ] || [ ! -e "$link_dir/herdr-reviewr" ]; then
      ln -sfn "$HERDR_PLUGIN_ROOT/bin/herdr-reviewr" "$link_dir/herdr-reviewr" 2>/dev/null || :
    fi
  done
fi

# Event policy gates the event alone: explicit actions ignore it. This is after validation but
# before workspace or pane inspection, so a disabled event performs no normal work.
if [ "$mode" = auto-open ]; then
  [ "$auto_open" = "false" ] && exit 0
  if [ "$placement" != "split" ] && [ "$placement" != "tab" ]; then
    exit 0
  fi
fi

refuse() {
  [ "$mode" = auto-open ] && exit 0
  printf 'reviewr: %s\n' "$1" >&2
  exit 1
}

ws="${HERDR_WORKSPACE_ID:-}"
pane="${HERDR_PANE_ID:-}"
cwd=""
[ -n "${HERDR_PLUGIN_CONTEXT_JSON:-}" ] &&
  cwd=$(printf '%s' "$HERDR_PLUGIN_CONTEXT_JSON" | jq -r '.focused_pane_cwd // .workspace_cwd // empty' 2>/dev/null)

# The event fires without a focused pane; target the fresh workspace from its payload
# (worktree.created shape: .data.workspace.workspace_id, .data.workspace.worktree.checkout_path).
if [ "$mode" = auto-open ] && [ -n "${HERDR_PLUGIN_EVENT_JSON:-}" ]; then
  ev="$HERDR_PLUGIN_EVENT_JSON"
  ws=$(printf '%s' "$ev" | jq -r '.data.workspace.workspace_id // .data.worktree.open_workspace_id // empty' 2>/dev/null)
  cwd=$(printf '%s' "$ev" | jq -r '.data.workspace.worktree.checkout_path // .data.worktree.path // empty' 2>/dev/null)
  pane=""
fi

[ -n "$ws" ] || refuse "no workspace context (invoke from inside herdr)"

# One pane-list snapshot serves the whole run. A failed or unreadable listing must not read
# as "no reviewr pane" — that would stack a duplicate on toggle and false-succeed a close.
panes_json=$("$H" pane list --workspace "$ws" 2>/dev/null) && [ -n "$panes_json" ] &&
  printf '%s' "$panes_json" | jq -e '.result.panes' >/dev/null 2>&1 ||
  refuse "herdr pane list failed for $ws"

# A reviewr pane runs the review UI in its foreground process group (specs/herdr-host.md,
# Pane identity). A wrapped launch (`cargo run`) counts through its child; a non-UI run
# (`--resolve-plugin-config` anywhere in argv, or the `nav` subcommand as the first
# argument) never counts. The executable name in `argv0`/`argv[0]` decides, never `name`:
# that field is a rewritable process title (docs/herdr-api-notes.md).
# Each exclusion mirrors its dispatch in `src/main.rs` — the flag is recognized anywhere
# in argv, the subcommand only in first position — and a future non-UI invocation must
# land in both halves.
# Takes the pane id and a scratch file for the call's stderr, which stays out of the JSON
# handed to jq — a successful call with an advisory on stderr must not read as unreadable.
# Returns 1 for a pane that is not the review UI — a pane the read reports gone exited
# between the list and this read and converges like any observed-then-exited pane — and 2
# for a read that fails any other way, which the caller refuses like a failed pane list.
is_reviewr_pane() {
  if ! info=$("$H" pane process-info --pane "$1" 2>"$2"); then
    case "$(cat "$2" 2>/dev/null)" in
    *pane_not_found*) return 1 ;;
    *) return 2 ;;
    esac
  fi
  # No `?` after the key path: an envelope missing `foreground_processes` is a shape
  # failure and must refuse, never read as "no reviewr pane" — only a present-but-empty
  # process list may count zero.
  count=$(printf '%s' "$info" | jq -r '
    def base: split("/") | last;
    [.result.process_info.foreground_processes[]
      | select((((.argv0 // "") | base) == "herdr-reviewr")
          or ((((.argv // [])[0] // "") | base) == "herdr-reviewr"))
      | select(((.argv // []) | index("--resolve-plugin-config")) == null)
      | select((((.argv // [])[1] // "")) != "nav")]
    | length' 2>/dev/null) || return 2
  # An empty count is a read that failed some new way, so the `[` error's status 2 refuses
  # it — a default here would pick the one outcome the failure semantics forbid.
  [ "$count" -gt 0 ] 2>/dev/null
}

# The workspace's reviewr panes: any tab, any placement, however they were launched
# (HH-LAUNCHER-BLIND) — the actions and a layout's own pane converge on one set. The
# per-pane reads run concurrently, so the sweep costs one process-info round-trip of
# wall clock, not one per pane in the workspace. Each probe reports through a marker
# file keyed by its position in the list, so nothing rests on a pane id's spelling.
pane_list=$(printf '%s' "$panes_json" | jq -r '.result.panes[].pane_id // empty' 2>/dev/null)
probe_dir=$(mktemp -d) || refuse "cannot create a temp dir"
trap 'rm -rf "$probe_dir"' EXIT
i=0
while IFS= read -r p; do
  [ -n "$p" ] || continue
  {
    is_reviewr_pane "$p" "$probe_dir/$i.err"
    case $? in
    0) : >"$probe_dir/$i.reviewr" ;;
    2) : >"$probe_dir/$i.unreadable" ;;
    esac
    # Written last, so its presence means the probe settled. A probe that never reports —
    # a failed fork or marker write — must refuse, never read as "not a reviewr pane".
    : >"$probe_dir/$i.done"
  } &
  i=$((i + 1))
done <<EOF
$pane_list
EOF
wait

existing=""
unreadable=0
i=0
while IFS= read -r p; do
  [ -n "$p" ] || continue
  [ -e "$probe_dir/$i.done" ] || unreadable=1
  [ -e "$probe_dir/$i.unreadable" ] && unreadable=1
  [ -e "$probe_dir/$i.reviewr" ] && existing="$existing$p"$'\n'
  i=$((i + 1))
done <<EOF
$pane_list
EOF
[ "$unreadable" -eq 0 ] || refuse "herdr pane process-info failed in $ws"

# Plain `pane close`, not `plugin pane close`: the live process read reaches a pane the
# plugin-pane registry forgot after a herdr restart, and a layout-launched pane was never
# in that registry at all (specs/herdr-host.md). A close refused because the pane is gone
# lost a benign race — the pane exited between the read and the close, the same end
# state — so the sweep still converges and exits 0. A close failing any other way names a
# pane that may still be running, so the sweep finishes the rest and then refuses rather
# than reporting that pane closed (specs/herdr-host.md, Failure semantics).
close_all() {
  closed=""
  failed=""
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if err=$("$H" pane close "$p" 2>&1 >/dev/null); then
      closed="$closed $p"
    else
      case "$err" in
      *pane_not_found*) closed="$closed $p" ;;
      *) failed="$failed $p" ;;
      esac
    fi
  done <<EOF
$existing
EOF
  [ -z "$failed" ] || refuse "herdr pane close failed for$failed in $ws"
  printf 'closed%s in %s\n' "$closed" "$ws"
}

case "$mode" in
close)
  [ -n "$existing" ] || { printf 'close: nothing open in %s\n' "$ws"; exit 0; }
  close_all
  exit 0
  ;;
toggle)
  if [ -n "$existing" ]; then
    close_all
    exit 0
  fi
  ;;
open | auto-open)
  if [ -n "$existing" ]; then
    [ "$mode" = open ] && printf 'open: already open (%s) in %s\n' "$(printf '%s' "$existing" | tr '\n' ' ' | sed 's/ $//')" "$ws"
    exit 0
  fi
  ;;
*)
  refuse "unknown mode '$mode' (toggle | open | close | auto-open)"
  ;;
esac

# Opening from here on. Prefer the focused pane's live `foreground_cwd`, read from the
# pane-list snapshot already in hand, over the context's launch cwd (specs/herdr-host.md,
# Repo discovery; the launch-vs-live split is docs/herdr-api-notes.md). The mode guard
# keeps auto-open on the event payload's cwd set above.
is_git_repo() { [ -n "$1" ] && git -C "$1" rev-parse --show-toplevel >/dev/null 2>&1; }
live=""
if [ "$mode" != auto-open ] && [ -n "${HERDR_PLUGIN_CONTEXT_JSON:-}" ]; then
  fp=$(printf '%s' "$HERDR_PLUGIN_CONTEXT_JSON" | jq -r '.focused_pane_id // empty' 2>/dev/null)
  [ -z "$fp" ] || live=$(printf '%s' "$panes_json" |
    jq -r --arg p "$fp" 'first(.result.panes[] | select(.pane_id == $p) | .foreground_cwd // empty)' 2>/dev/null)
fi
if is_git_repo "$live"; then
  cwd="$live"
elif ! is_git_repo "$cwd"; then
  # Name every candidate the check rejected, or a refusal over an inspected-but-unusable
  # live cwd would read as if no directory was ever tried.
  refuse "not a git repo: '${cwd:-<no cwd>}'${live:+ (live cwd '$live')}"
fi

# A manual open takes focus. The event never does (specs/herdr-host.md).
focus=--no-focus
[ "$mode" != auto-open ] && focus=--focus

# Placement decides the pane-open shape (spec: Pane placement). A split or zoomed
# open attaches to the focused pane, else the workspace's first pane.
case "$placement" in
split | zoomed)
  if [ -z "$pane" ]; then
    pane=$(printf '%s' "$panes_json" | jq -r '.result.panes[0].pane_id // empty' 2>/dev/null)
  fi
  [ -n "$pane" ] || refuse "no pane to attach to in $ws"
  set -- --placement "$placement" --target-pane "$pane"
  [ "$placement" = "split" ] && set -- "$@" --direction "$direction"
  ;;
tab)
  set -- --placement tab --workspace "$ws"
  ;;
overlay)
  set -- --placement overlay
  ;;
*)
  refuse "unreachable placement '$placement'" # guard against a future value leaking $@
  ;;
esac

open_json=$("$H" plugin pane open --plugin "${HERDR_PLUGIN_ID:-persiyanov.reviewr}" --entrypoint pane \
  "$@" --cwd "$cwd" "$focus" 2>/dev/null)
new=$(printf '%s' "$open_json" | jq -r '.result.plugin_pane.pane.pane_id // empty' 2>/dev/null)
[ -n "$new" ] || refuse "herdr plugin pane open failed"

# A tab open lands in a fresh tab that herdr labels with a bare index; name it
# after the plugin so the tab bar reads "reviewr" (specs/herdr-host.md). Cosmetic: a
# failed rename never fails an open that already succeeded.
if [ "$placement" = tab ]; then
  tab=$(printf '%s' "$open_json" | jq -r '.result.plugin_pane.pane.tab_id // empty' 2>/dev/null)
  [ -z "$tab" ] || "$H" tab rename "$tab" reviewr >/dev/null 2>&1
fi
[ "$mode" = auto-open ] || printf 'opened %s (%s) in %s\n' "$new" "$placement" "$ws"
