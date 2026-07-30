# herdr `[[build]]` step, Windows twin of install.sh: download the prebuilt herdr-reviewr
# binary for this platform from the matching GitHub Release into the plugin's bin/ dir. Runs
# on `herdr plugin install` (a managed checkout); `herdr plugin link` skips the build step --
# for a local checkout, build from source with `cargo install --path .`.
# ASCII only: PowerShell 5.1 reads a BOM-less script as ANSI, and a UTF-8 em-dash's trailing
# byte decodes to a curly quote that PS treats as a string delimiter.
#
# The build runs with the plugin checkout as the working directory, but like install.sh we
# resolve the plugin root from this script's location rather than $env:HERDR_PLUGIN_ROOT
# (build commands may not receive the runtime env; and on Windows herdr hands out the plugin
# root as a `\\?\` verbatim path). At runtime the pane command reads
# $HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe.
$ErrorActionPreference = 'Stop'

$Name = 'herdr-reviewr'
$Repo = 'dcieslak19973/herdr-reviewr'

$Root = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Root 'bin'

# The release tag matches the manifest version, so a checkout always pulls its own release.
$versionLine = (Get-Content (Join-Path $Root 'herdr-plugin.toml')) -match '^version' |
    Select-Object -First 1
if (-not ($versionLine -match '"([^"]+)"')) {
    throw "${Name}: cannot read version from herdr-plugin.toml"
}
$Tag = "v$($Matches[1])"

# One prebuilt Windows target. Windows-on-ARM runs it via x64 emulation; anything genuinely
# 32-bit gets the same source-build escape hatch as an unmapped unix platform.
if (-not [Environment]::Is64BitOperatingSystem) {
    throw "${Name}: no prebuilt binary for 32-bit Windows -- build from source with 'cargo install --path .'"
}
$Target = 'x86_64-pc-windows-msvc'

# taiki-e's Windows archives are .zip; the checksum sidecar drops the archive extension:
# <name>-<target>.sha256, not <archive>.sha256.
$Archive = "$Name-$Target.zip"
$Checksum = "$Name-$Target.sha256"
# HERDR_REVIEWR_BASE_URL is a test hook: point it at a local directory of staged assets and
# Get-Asset copies instead of downloading.
$Base = if ($env:HERDR_REVIEWR_BASE_URL) { $env:HERDR_REVIEWR_BASE_URL }
        else { "https://github.com/$Repo/releases/download/$Tag" }

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    # Release-asset downloads are eventually-consistent: GitHub's CDN can 404 for a few
    # minutes after a release publishes, even though the asset exists. Retry (incl. on 404)
    # so an install right after a release doesn't fail spuriously.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    function Get-Asset([string]$Source, [string]$Dest) {
        if (Test-Path $Source) { Copy-Item $Source $Dest; return }
        for ($attempt = 1; $attempt -le 5; $attempt++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri $Source -OutFile $Dest
                return
            } catch {
                if ($attempt -eq 5) { throw }
                Start-Sleep -Seconds 3
            }
        }
    }

    Write-Output "${Name}: downloading $Archive ($Tag)"
    Get-Asset "$Base/$Archive" (Join-Path $Tmp $Archive)
    Get-Asset "$Base/$Checksum" (Join-Path $Tmp $Checksum)

    Write-Output "${Name}: verifying checksum"
    $expected = ((Get-Content (Join-Path $Tmp $Checksum) -TotalCount 1) -split '\s+')[0]
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $Tmp $Archive)).Hash
    # -ne on strings is case-insensitive in PowerShell, which is what we want: the sidecar
    # is lowercase hex, Get-FileHash reports uppercase.
    if ($expected -ne $actual) {
        throw "${Name}: checksum mismatch (expected $expected, got $actual)"
    }

    Expand-Archive -Path (Join-Path $Tmp $Archive) -DestinationPath $Tmp -Force
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item -Path (Join-Path $Tmp "$Name.exe") -Destination (Join-Path $BinDir "$Name.exe") -Force
    Write-Output "${Name}: installed $(Join-Path $BinDir "$Name.exe")"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

# No PATH mutation on Windows (there is no ~/.local/bin convention): the pane and actions
# invoke the binary by absolute path under the plugin root, and a shell can too.
Write-Output "${Name}: run it directly as: & '$(Join-Path $BinDir "$Name.exe")'"

# Post-install next steps: printed on success only, never affects exit status.
Write-Output "${Name}: next steps"
Write-Output "  1) install the agent skill:  npx skills add dcieslak19973/herdr-reviewr --skill reviewr-comments -g"
Write-Output "     (or: & '$(Join-Path $BinDir "$Name.exe")' skill-install . or: herdr plugin action invoke skill-install-windows --plugin dcieslak19973.reviewr)"
