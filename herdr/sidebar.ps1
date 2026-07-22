# The reviewr sidebar actions and event hook (Windows port of herdr/sidebar.sh).
#
#   sidebar.ps1 toggle      open the sidebar, or close it if open
#   sidebar.ps1 open        open the sidebar, no-op if one is open
#   sidebar.ps1 close       close every reviewr pane, no-op if none
#   sidebar.ps1 auto-open   worktree.created hook: open, gated by auto_open and placement
#
# The workspace's sidebar is any pane labeled "reviewr" in the live pane list. There is no state
# file. Actions refuse loudly (exit 1, one stderr line) and report successes on stdout; a refused
# event reports its config error through stderr for herdr's plugin log.
#
# PowerShell parses JSON natively, so unlike the unix script this needs no jq. On Windows herdr's
# `bash` resolves to WSL, which can't see the Windows repo — hence a native PowerShell port. Each
# step is tolerant: a transient herdr/git hiccup must never read as "no sidebar".
[CmdletBinding()]
param([string]$Mode = 'toggle')

$ErrorActionPreference = 'SilentlyContinue'

function Herdr { & $script:H @args 2>$null }

# Refuse loudly for explicit actions; a refused event stays silent (exit 0) so the plugin log
# isn't spammed on every worktree.created that isn't a git repo.
function Refuse([string]$msg) {
    if ($Mode -eq 'auto-open') { exit 0 }
    [Console]::Error.WriteLine("reviewr: $msg")
    exit 1
}

$script:H = if ($env:HERDR_BIN_PATH) { $env:HERDR_BIN_PATH } else { 'herdr' }
$pluginId = if ($env:HERDR_PLUGIN_ID) { $env:HERDR_PLUGIN_ID } else { 'persiyanov.reviewr' }

# Resolve the reviewr binary exactly as sidebar.sh does: explicit override, else under the plugin
# root, else PATH. Strip any \\?\ extended-length prefix from the plugin root so the .exe resolves.
if ($env:HERDR_REVIEWR_BIN) {
    $reviewr = $env:HERDR_REVIEWR_BIN
}
elseif ($env:HERDR_PLUGIN_ROOT) {
    $root = $env:HERDR_PLUGIN_ROOT -replace '^\\\\\?\\', ''
    $reviewr = Join-Path $root 'bin\herdr-reviewr.exe'
}
else {
    $reviewr = 'herdr-reviewr'
}

# Validate the whole plugin config before reading workspace state or taking any action. The Rust
# binary owns TOML parsing and defaults, so every plugin entry point shares exactly one contract.
$configJson = (& $reviewr --resolve-plugin-config 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0) {
    $text = $configJson.Trim()
    if (-not $text) { $text = 'reviewr: configuration validation failed' }
    [Console]::Error.WriteLine($text)
    exit 1
}
try {
    $config = ($configJson | ConvertFrom-Json)
}
catch {
    [Console]::Error.WriteLine('reviewr: normalized configuration is unreadable')
    exit 1
}
$placement = $config.toggle_placement
$direction = $config.toggle_direction
$autoOpen = $config.auto_open
if (-not $placement -or -not $direction -or $null -eq $autoOpen) {
    [Console]::Error.WriteLine('reviewr: normalized configuration is unreadable')
    exit 1
}

# Event policy gates the event alone: explicit actions ignore it. This is after validation but
# before workspace or pane inspection, so a disabled event performs no normal work.
if ($Mode -eq 'auto-open') {
    if (-not $autoOpen) { exit 0 }
    if ($placement -ne 'split' -and $placement -ne 'tab') { exit 0 }
}

$ws = $env:HERDR_WORKSPACE_ID
$pane = $env:HERDR_PANE_ID
$cwd = ''

# Focused-pane context (manual action): prefer the focused pane's cwd, else the workspace cwd.
if ($env:HERDR_PLUGIN_CONTEXT_JSON) {
    try {
        $ctx = $env:HERDR_PLUGIN_CONTEXT_JSON | ConvertFrom-Json
        if ($ctx.focused_pane_cwd) { $cwd = $ctx.focused_pane_cwd }
        elseif ($ctx.workspace_cwd) { $cwd = $ctx.workspace_cwd }
    }
    catch {}
}

# The event fires without a focused pane; target the fresh workspace from its payload
# (worktree.created shape: .data.workspace.workspace_id, .data.workspace.worktree.checkout_path).
if ($Mode -eq 'auto-open' -and $env:HERDR_PLUGIN_EVENT_JSON) {
    try {
        $d = ($env:HERDR_PLUGIN_EVENT_JSON | ConvertFrom-Json).data
        if ($d.workspace.workspace_id) { $ws = $d.workspace.workspace_id }
        elseif ($d.worktree.open_workspace_id) { $ws = $d.worktree.open_workspace_id }
        if ($d.workspace.worktree.checkout_path) { $cwd = $d.workspace.worktree.checkout_path }
        elseif ($d.worktree.path) { $cwd = $d.worktree.path }
        $pane = ''
    }
    catch {}
}

if (-not $ws) { Refuse 'no workspace context (invoke from inside herdr)' }

# One pane-list snapshot serves the whole run. A failed listing must not read as "no sidebar" —
# that would stack a duplicate on toggle and false-succeed a close.
$panesJson = (Herdr pane list --workspace $ws | Out-String).Trim()
if (-not $panesJson) { Refuse "herdr pane list failed for $ws" }
try {
    $panes = ($panesJson | ConvertFrom-Json).result.panes
}
catch {
    Refuse "herdr pane list failed for $ws"
}

# The workspace's sidebar: every reviewr-labeled pane, any tab, any placement.
$existing = @()
if ($panes) {
    $existing = @($panes | Where-Object { $_.label -eq 'reviewr' } | ForEach-Object { $_.pane_id })
}

# Plain `pane close`, not `plugin pane close`: the plugin-pane registry does not survive a herdr
# restart and would strand the pane.
function Close-All {
    $closed = @()
    $failed = @()
    foreach ($p in $existing) {
        if (-not $p) { continue }
        Herdr pane close $p | Out-Null
        if ($LASTEXITCODE -eq 0) { $closed += $p } else { $failed += $p }
    }
    if ($failed.Count -gt 0) { Refuse "failed to close $($failed -join ' ') in $ws" }
    "closed $($closed -join ' ') in $ws"
}

if ($Mode -eq 'close') {
    if ($existing.Count -eq 0) { "close: nothing open in $ws"; exit 0 }
    Close-All
    exit 0
}
elseif ($Mode -eq 'toggle') {
    if ($existing.Count -gt 0) { Close-All; exit 0 }
}
elseif ($Mode -eq 'open' -or $Mode -eq 'auto-open') {
    if ($existing.Count -gt 0) {
        if ($Mode -eq 'open') { "open: already open ($($existing -join ' ')) in $ws" }
        exit 0
    }
}
else {
    Refuse "unknown mode '$Mode' (toggle | open | close | auto-open)"
}

# Opening from here on. Only inside a git repo.
$isRepo = $false
if ($cwd) {
    & git -C $cwd rev-parse --show-toplevel 2>$null | Out-Null
    if ($LASTEXITCODE -eq 0) { $isRepo = $true }
}
if (-not $isRepo) {
    $shown = if ($cwd) { $cwd } else { '<no cwd>' }
    Refuse "not a git repo: '$shown'"
}

# Focus follows the placement on a manual open; the event never takes it.
$focus = '--no-focus'
if ($Mode -ne 'auto-open' -and $placement -ne 'split') { $focus = '--focus' }

# Placement decides the pane-open shape. A split or zoomed open attaches to the focused pane, else
# the workspace's first pane.
$openArgs = @()
if ($placement -eq 'split' -or $placement -eq 'zoomed') {
    if (-not $pane -and $panes) { $pane = $panes[0].pane_id }
    if (-not $pane) { Refuse "no pane to attach to in $ws" }
    $openArgs = @('--placement', $placement, '--target-pane', $pane)
    if ($placement -eq 'split') { $openArgs += @('--direction', $direction) }
}
elseif ($placement -eq 'tab') {
    $openArgs = @('--placement', 'tab', '--workspace', $ws)
}
elseif ($placement -eq 'overlay') {
    $openArgs = @('--placement', 'overlay')
}
else {
    Refuse "unreachable placement '$placement'"
}

$openJson = (Herdr plugin pane open --plugin $pluginId --entrypoint sidebar-win `
        @openArgs --cwd $cwd $focus | Out-String).Trim()
$new = ''
if ($openJson) {
    try { $new = ($openJson | ConvertFrom-Json).result.plugin_pane.pane.pane_id } catch {}
}
if (-not $new) { Refuse 'herdr plugin pane open failed' }
if ($Mode -ne 'auto-open') { "opened $new ($placement) in $ws" }
