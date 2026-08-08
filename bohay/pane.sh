#!/usr/bin/env bash
# The reviewr pane actions and event hook for bohay.
#
#   pane.sh toggle      open a reviewr pane, or close every one if any is open
#   pane.sh open        open a reviewr pane, no-op if one is open
#   pane.sh close       close every reviewr pane, no-op if none
#   pane.sh auto-open   workspace.created hook: open, gated by the auto_open setting
#
# A reviewr pane is any pane bohay opened for this module's `pane` entrypoint, read
# from `bohay pane list` (each pane carries its {module.id, module.entrypoint}).
# That is exact, so there is no process inspection here. Actions report successes on
# stdout and refuse on stderr with a non-zero exit.
set -uo pipefail

# bohay runs module commands with a minimal PATH; ensure jq/git/bohay resolve.
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:${PATH:-}"

mode="${1:-toggle}"
B="${BOHAY_BIN_PATH:-bohay}"
MODULE_ID="${BOHAY_MODULE_ID:-persiyanov.reviewr}"
ENTRYPOINT="pane"
PLACEMENT="${BOHAY_SETTING_PLACEMENT:-split}"

refuse() {
  printf 'reviewr: %s\n' "$1" >&2
  exit 1
}

# The workspace's open reviewr panes, one pane id per line. A failed or unreadable
# listing must not read as "nothing open" (that would stack a duplicate on toggle and
# false-succeed a close), so an unparseable list refuses.
list_reviewr() {
  local out
  out=$("$B" pane list 2>/dev/null) || refuse "bohay pane list failed"
  printf '%s' "$out" | jq -e '.result.panes' >/dev/null 2>&1 || refuse "bohay pane list unreadable"
  printf '%s' "$out" | jq -r --arg id "$MODULE_ID" --arg ep "$ENTRYPOINT" \
    '.result.panes[] | select(.module.id == $id and .module.entrypoint == $ep) | .pane' 2>/dev/null
}

open_pane() {
  "$B" module pane open "$MODULE_ID" "$ENTRYPOINT" --placement "$PLACEMENT" >/dev/null 2>&1 ||
    refuse "bohay module pane open failed"
  printf 'opened reviewr (%s)\n' "$PLACEMENT"
}

close_all() {
  local closed=""
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    "$B" pane close "$p" >/dev/null 2>&1 && closed="$closed $p"
  done <<EOF
$1
EOF
  printf 'closed%s\n' "${closed:- nothing}"
}

case "$mode" in
open)
  existing="$(list_reviewr)"
  [ -n "$existing" ] && { printf 'open: already open\n'; exit 0; }
  open_pane
  ;;
close)
  existing="$(list_reviewr)"
  [ -n "$existing" ] || { printf 'close: nothing open\n'; exit 0; }
  close_all "$existing"
  ;;
toggle)
  existing="$(list_reviewr)"
  if [ -n "$existing" ]; then
    close_all "$existing"
  else
    open_pane
  fi
  ;;
auto-open)
  # Opt-in only, and never a second pane.
  [ "${BOHAY_SETTING_AUTO_OPEN:-false}" = "true" ] || exit 0
  existing="$(list_reviewr)"
  [ -n "$existing" ] && exit 0
  open_pane
  ;;
*)
  refuse "unknown mode '$mode' (toggle | open | close | auto-open)"
  ;;
esac
