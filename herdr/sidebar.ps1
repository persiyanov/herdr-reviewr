# Open / toggle the reviewr sidebar as a right split (Windows port of sidebar.sh).
# Invoked by herdr with the plugin runtime env set (HERDR_BIN_PATH, HERDR_PANE_ID,
# HERDR_WORKSPACE_ID, HERDR_PLUGIN_*, HERDR_PLUGIN_CONTEXT_JSON, and HERDR_PLUGIN_EVENT_JSON for
# events).
#
#   sidebar.ps1 toggle   key action: open the sidebar, or close it if already open
#   sidebar.ps1 open     event hook: open the sidebar if not already open (e.g. worktree.created)
#
# No hard failures: a transient herdr/git hiccup must not abort the toggle; each step is tolerant
# and results are checked explicitly. PowerShell parses JSON natively, so unlike the unix script
# this needs no jq. On Windows herdr's `bash` resolves to WSL, which can't see the Windows repo —
# hence a native PowerShell script rather than a shared shell one.
[CmdletBinding()]
param([string]$Mode = 'toggle')

$ErrorActionPreference = 'SilentlyContinue'

function Herdr { & $script:H @args 2>$null }

$script:H = if ($env:HERDR_BIN_PATH) { $env:HERDR_BIN_PATH } else { 'herdr' }
$pluginId = if ($env:HERDR_PLUGIN_ID) { $env:HERDR_PLUGIN_ID } else { 'persiyanov.reviewr' }

$ws   = $env:HERDR_WORKSPACE_ID
$pane = $env:HERDR_PANE_ID
$cwd  = ''

# Focused-pane context (key action): prefer the focused pane's cwd, else the workspace cwd.
if ($env:HERDR_PLUGIN_CONTEXT_JSON) {
    try {
        $ctx = $env:HERDR_PLUGIN_CONTEXT_JSON | ConvertFrom-Json
        if     ($ctx.focused_pane_cwd) { $cwd = $ctx.focused_pane_cwd }
        elseif ($ctx.workspace_cwd)    { $cwd = $ctx.workspace_cwd }
    } catch {}
}

# An event fires without a focused pane; target the new worktree's workspace from the payload
# (worktree.created shape: .data.workspace.workspace_id, .data.workspace.worktree.checkout_path).
if ($env:HERDR_PLUGIN_EVENT_JSON) {
    try {
        $d = ($env:HERDR_PLUGIN_EVENT_JSON | ConvertFrom-Json).data
        if     ($d.workspace.workspace_id)     { $ws = $d.workspace.workspace_id }
        elseif ($d.worktree.open_workspace_id) { $ws = $d.worktree.open_workspace_id }
        if     ($d.workspace.worktree.checkout_path) { $cwd = $d.workspace.worktree.checkout_path }
        elseif ($d.worktree.path)                    { $cwd = $d.worktree.path }
        $pane = ''
    } catch {}
}

# A workspace is required to key state and target the split; without it, do nothing rather than
# collide every workspace on a shared state file.
if (-not $ws) { exit 0 }

$stateDir = if ($env:HERDR_PLUGIN_STATE_DIR) { $env:HERDR_PLUGIN_STATE_DIR } else { $env:TEMP }
New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
$state = Join-Path $stateDir "pane-$ws"

# Is a sidebar we opened still alive in this workspace?
$existing = ''
if (Test-Path $state) {
    $prev = (Get-Content $state -Raw -ErrorAction SilentlyContinue)
    if ($prev) { $prev = $prev.Trim() }
    if ($prev) {
        $panes = (Herdr pane list --workspace $ws | ConvertFrom-Json).result.panes
        if ($panes | Where-Object { $_.pane_id -eq $prev }) { $existing = $prev }
    }
    if (-not $existing) { Remove-Item $state -ErrorAction SilentlyContinue } # stale (closed via `q`)
}

# Already open: toggle closes it; open is idempotent (don't stack a duplicate pane).
if ($existing) {
    if ($Mode -eq 'toggle') {
        Herdr plugin pane close $existing | Out-Null
        Remove-Item $state -ErrorAction SilentlyContinue
    }
    exit 0
}

# Only open inside a git repo.
if (-not $cwd) { exit 0 }
& git -C $cwd rev-parse --show-toplevel 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) { exit 0 }

# A split plugin pane must target an existing pane; for an event (no focused pane), use the target
# workspace's first pane.
if (-not $pane) {
    $panes = (Herdr pane list --workspace $ws | ConvertFrom-Json).result.panes
    if ($panes) { $pane = $panes[0].pane_id }
}
if (-not $pane) { exit 0 }

$new = (Herdr plugin pane open --plugin $pluginId --entrypoint sidebar-win `
    --placement split --direction right --target-pane $pane --cwd $cwd --no-focus |
    ConvertFrom-Json).result.plugin_pane.pane.pane_id
if ($new) { Set-Content -Path $state -Value $new -NoNewline }
