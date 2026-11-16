# Run herdr/install.ps1, as the manifest's build entry does, against a local fixture of the release
# action's zip and .sha256: <this> -Archive <zip> -Checksum <sha256>
param(
    [Parameter(Mandatory = $true)] [string] $Archive,
    [Parameter(Mandatory = $true)] [string] $Checksum
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$checkout = Split-Path -Parent $PSScriptRoot
$Archive = (Resolve-Path -LiteralPath $Archive).Path
$Checksum = (Resolve-Path -LiteralPath $Checksum).Path
$versionLine = Get-Content -LiteralPath (Join-Path $checkout 'herdr-plugin.toml') |
    Where-Object { $_ -match '^version' } | Select-Object -First 1
$null = $versionLine -match '"([^"]+)"'
$tag = "v$($Matches[1])"

$work = Join-Path ([IO.Path]::GetTempPath()) "reviewr-install-check-$([guid]::NewGuid().ToString('N'))"
$port = 8765
$failures = @()

function Fail-Row([string]$Row, [string]$Why) {
    $script:failures += "${Row}: $Why"
    Write-Host "FAIL ${Row}: $Why"
}

# One release dir per row under the served root: <root>\<row>\<tag>\<assets>.
function New-Release([string]$Row, [string]$SidecarText) {
    $dir = Join-Path $work "releases\$Row\$tag"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -LiteralPath $Archive -Destination $dir
    $sidecar = Join-Path $dir (Split-Path -Leaf $Checksum)
    if ($SidecarText) {
        Set-Content -LiteralPath $sidecar -Value $SidecarText -Encoding ascii
    } else {
        Copy-Item -LiteralPath $Checksum -Destination $sidecar
    }
    return "http://127.0.0.1:$port/$Row"
}

# A plugin root holding only what install.ps1 reads, with a stand-in binary already in bin\.
function New-PluginRoot([string]$Row) {
    $root = Join-Path $work "plugins\$Row"
    New-Item -ItemType Directory -Force -Path (Join-Path $root 'herdr'), (Join-Path $root 'bin') | Out-Null
    Copy-Item -LiteralPath (Join-Path $checkout 'herdr-plugin.toml') -Destination $root
    Copy-Item -LiteralPath (Join-Path $checkout 'herdr\install.ps1') -Destination (Join-Path $root 'herdr')
    Set-Content -LiteralPath (Join-Path $root 'bin\herdr-reviewr.exe') -Value 'previous build' -Encoding ascii
    return $root
}

# Start the manifest's Windows build argv in `Root` against `BaseUrl`: the staged copy's release
# URL is the one line that differs from what users run.
function Start-Install([string]$Root, [string]$BaseUrl) {
    $script = Join-Path $Root 'herdr\install.ps1'
    $github = '$BaseUrl = "https://github.com/$Repo/releases/download"'
    $text = Get-Content -LiteralPath $script -Raw
    if (-not $text.Contains($github)) { throw "install.ps1 no longer names its release URL as $github" }
    Set-Content -LiteralPath $script -Value $text.Replace($github, "`$BaseUrl = '$BaseUrl'") -Encoding ascii -NoNewline
    $proc = Start-Process -FilePath 'powershell' -WorkingDirectory $Root -NoNewWindow -PassThru `
        -RedirectStandardOutput (Join-Path $Root 'stdout.txt') `
        -RedirectStandardError (Join-Path $Root 'stderr.txt') `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'herdr/install.ps1')
    # 5.1 reports no ExitCode for a process whose handle was never read before it exited.
    $null = $proc.Handle
    return $proc
}

# Wait for the install `Proc` in `Root`. Returns .Code and .Lines (stdout, then stderr).
function Wait-Install($Proc, [string]$Root) {
    $Proc.WaitForExit()
    $lines = @(Get-Content -LiteralPath (Join-Path $Root 'stdout.txt')) +
        @(Get-Content -LiteralPath (Join-Path $Root 'stderr.txt')) | Where-Object { $_ }
    $lines | ForEach-Object { Write-Host "  | $_" }
    return [pscustomobject]@{ Code = $Proc.ExitCode; Lines = @($lines) }
}

function Invoke-Install([string]$Root, [string]$BaseUrl) {
    return Wait-Install (Start-Install $Root $BaseUrl) $Root
}

function Assert-Prefixed([string]$Row, $Lines) {
    $stray = @($Lines | Where-Object { -not $_.StartsWith('reviewr: ') })
    if ($stray.Count -gt 0) { Fail-Row $Row "lines without the 'reviewr: ' prefix: $($stray -join ' / ')" }
}

New-Item -ItemType Directory -Force -Path (Join-Path $work 'releases') | Out-Null
$goodUrl = New-Release 'good' $null
$badUrl = New-Release 'bad' ("0" * 64 + " *" + (Split-Path -Leaf $Archive))

$server = Start-Process -FilePath 'python' -PassThru -WindowStyle Hidden `
    -ArgumentList @('-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', (Join-Path $work 'releases'))
try {
    $deadline = (Get-Date).AddSeconds(15)
    while ($true) {
        try { Invoke-WebRequest -UseBasicParsing -Uri "$goodUrl/$tag/$(Split-Path -Leaf $Checksum)" | Out-Null; break } catch {}
        if ((Get-Date) -ge $deadline) { throw 'fixture server did not start' }
        Start-Sleep -Milliseconds 250
    }

    Write-Host "row: checksum mismatch refuses and leaves bin\ untouched"
    $root = New-PluginRoot 'mismatch'
    $run = Invoke-Install $root $badUrl
    if ($run.Code -eq 0) { Fail-Row 'mismatch' 'exited 0' }
    if (-not ($run.Lines -match '^reviewr: checksum mismatch')) { Fail-Row 'mismatch' 'no checksum mismatch line' }
    Assert-Prefixed 'mismatch' $run.Lines
    $bin = @(Get-ChildItem -LiteralPath (Join-Path $root 'bin') | ForEach-Object { $_.Name })
    if (($bin -join ',') -ne 'herdr-reviewr.exe') { Fail-Row 'mismatch' "bin\ holds $($bin -join ', ')" }
    if ((Get-Content -LiteralPath (Join-Path $root 'bin\herdr-reviewr.exe')) -ne 'previous build') {
        Fail-Row 'mismatch' 'bin\herdr-reviewr.exe changed'
    }

    Write-Host "row: a verified release replaces bin\herdr-reviewr.exe"
    $root = New-PluginRoot 'success'
    $run = Invoke-Install $root $goodUrl
    if ($run.Code -ne 0) { Fail-Row 'success' "exited $($run.Code)" }
    Assert-Prefixed 'success' $run.Lines
    $expected = Join-Path $work 'expected'
    Expand-Archive -LiteralPath $Archive -DestinationPath $expected
    $want = (Get-FileHash -LiteralPath (Join-Path $expected 'herdr-reviewr.exe')).Hash
    $got = (Get-FileHash -LiteralPath (Join-Path $root 'bin\herdr-reviewr.exe')).Hash
    if ($want -ne $got) { Fail-Row 'success' "installed $got, the release holds $want" }
    $bin = @(Get-ChildItem -LiteralPath (Join-Path $root 'bin') | ForEach-Object { $_.Name })
    if (($bin -join ',') -ne 'herdr-reviewr.exe') { Fail-Row 'success' "bin\ holds $($bin -join ', ')" }

    # GitHub's CDN can 404 an asset for minutes after a release publishes. The install keeps
    # retrying, so a release that appears mid-install still installs.
    Write-Host "row: a release that 404s at first installs once it appears"
    $root = New-PluginRoot 'late'
    $proc = Start-Install $root "http://127.0.0.1:$port/late"
    Start-Sleep -Seconds 5
    $null = New-Release 'late' $null
    $run = Wait-Install $proc $root
    if ($run.Code -ne 0) { Fail-Row 'late' "exited $($run.Code)" }
    $got = (Get-FileHash -LiteralPath (Join-Path $root 'bin\herdr-reviewr.exe')).Hash
    if ($want -ne $got) { Fail-Row 'late' "installed $got, the release holds $want" }

    # herdr started from pwsh 7 hands the build pwsh's PSModulePath, PS7's module dirs first.
    Write-Host "row: a PSModulePath inherited from pwsh 7 still installs"
    $root = New-PluginRoot 'pwsh'
    $inherited = $env:PSModulePath
    $env:PSModulePath = & pwsh -NoProfile -Command '[Console]::Out.Write($env:PSModulePath)'
    try { $run = Invoke-Install $root $goodUrl } finally { $env:PSModulePath = $inherited }
    if ($run.Code -ne 0) { Fail-Row 'pwsh' "exited $($run.Code)" }
    Assert-Prefixed 'pwsh' $run.Lines
    $got = (Get-FileHash -LiteralPath (Join-Path $root 'bin\herdr-reviewr.exe')).Hash
    if ($want -ne $got) { Fail-Row 'pwsh' "installed $got, the release holds $want" }
} finally {
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
}

if ($failures.Count -gt 0) {
    throw "install.ps1 check failed:`n$($failures -join "`n")"
}
Write-Host 'install.ps1 check passed'
