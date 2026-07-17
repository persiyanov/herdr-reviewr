# herdr `[[build]]` step (Windows): download the prebuilt herdr-reviewr.exe for this platform from
# the matching GitHub Release into the plugin's bin/ dir. Runs on `herdr plugin install` (a managed
# checkout); `herdr plugin link` skips the build step — for a local checkout, build from source
# with `cargo build --release` and copy target\release\herdr-reviewr.exe into bin\.
#
# The build runs with the plugin checkout as the working directory, but we resolve the plugin root
# from this script's location rather than $HERDR_PLUGIN_ROOT (build commands may not receive the
# runtime env). This mirrors herdr/install.sh for macOS/Linux.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$Name = 'herdr-reviewr'
$Repo = 'persiyanov/herdr-reviewr'

$Root   = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Root 'bin'

# The release tag matches the manifest version, so a checkout always pulls its own release.
$manifest = Get-Content (Join-Path $Root 'herdr-plugin.toml') -Raw
$version  = [regex]::Match($manifest, '(?m)^\s*version\s*=\s*"([^"]+)"').Groups[1].Value
$tag      = "v$version"

# taiki-e uploads a .zip (with a .exe inside) for Windows targets; the .sha256 sidecar drops the
# archive extension: <name>-<target>.sha256, not <archive>.sha256.
$target   = 'x86_64-pc-windows-msvc'
$archive  = "$Name-$target.zip"
$checksum = "$Name-$target.sha256"
$base     = "https://github.com/$Repo/releases/download/$tag"

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("$Name-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    # Release-asset downloads are eventually-consistent: GitHub's CDN can 404 for a few minutes
    # after a release publishes even though the asset exists. Retry (incl. on 404).
    function Get-WithRetry([string]$Url, [string]$Dest) {
        for ($i = 1; $i -le 5; $i++) {
            try { Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing; return }
            catch { if ($i -eq 5) { throw }; Start-Sleep -Seconds 3 }
        }
    }

    Write-Host "$Name: downloading $archive ($tag)"
    Get-WithRetry "$base/$archive"  (Join-Path $tmp $archive)
    Get-WithRetry "$base/$checksum" (Join-Path $tmp $checksum)

    Write-Host "$Name: verifying checksum"
    $expected = (((Get-Content (Join-Path $tmp $checksum) -Raw).Trim()) -split '\s+')[0].ToLower()
    $actual   = (Get-FileHash (Join-Path $tmp $archive) -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "$Name: checksum mismatch (expected $expected, got $actual)"
    }

    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath $tmp -Force
    Copy-Item (Join-Path $tmp "$Name.exe") (Join-Path $BinDir "$Name.exe") -Force
    Write-Host "$Name: installed $(Join-Path $BinDir "$Name.exe")"
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
