# herdr's Windows `[[build]]` step, install.sh's twin: put the release's herdr-reviewr.exe in bin\.
# ASCII and PowerShell 5.1 only: 5.1 reads a BOM-less script in the ANSI code page.

$ErrorActionPreference = 'Stop'
# A PSModulePath inherited from pwsh 7 loads PS7's modules, which 5.1 can't run (Get-FileHash).
$env:PSModulePath = Join-Path $PSHOME 'Modules'
# 5.1 draws a progress bar per downloaded chunk, which slows a download severalfold.
$ProgressPreference = 'SilentlyContinue'
# 5.1 can default to TLS 1.0, which GitHub refuses.
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Name = 'herdr-reviewr'
# Every line this step prints starts with the plugin's name, as the action lines do.
$Say = 'reviewr'
$Repo = 'persiyanov/herdr-reviewr'

# The root from this script's place: herdr runs the build without the runtime env.
$Root = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Root 'bin'

function Fail([string]$Why) {
    [Console]::Error.WriteLine("${Say}: $Why")
    exit 1
}

# Release-asset downloads are eventually-consistent: GitHub's CDN can 404 for a few minutes
# after a release publishes, even though the asset exists. Retry every failure, 404 included.
function Get-Asset([string]$Url, [string]$Dest) {
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $Dest
            return
        } catch {
            $why = $_.Exception.Message
            Start-Sleep -Seconds 3
        }
    }
    Fail "could not download $Url ($why)"
}

# The release tag matches the manifest version, so a checkout always pulls its own release.
$versionLine = Get-Content -LiteralPath (Join-Path $Root 'herdr-plugin.toml') |
    Where-Object { $_ -match '^version' } | Select-Object -First 1
if (-not ($versionLine -match '"([^"]+)"')) {
    Fail 'cannot read version from herdr-plugin.toml'
}
$Tag = "v$($Matches[1])"

# One prebuilt Windows target. ARM64 Windows runs it under x64 emulation, as it runs herdr. A
# 32-bit PowerShell on 64-bit Windows reports the OS's architecture in PROCESSOR_ARCHITEW6432.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
    Fail "no prebuilt binary for Windows-$arch, build with 'cargo build --release' and copy herdr-reviewr.exe into bin\"
}
$Target = 'x86_64-pc-windows-msvc'

$Archive = "$Name-$Target.zip"
# taiki-e's checksum sidecar drops the archive extension: <name>-<target>.sha256.
$Checksum = "$Name-$Target.sha256"
$BaseUrl = "https://github.com/$Repo/releases/download"
$Base = "$BaseUrl/$Tag"

$Tmp = Join-Path ([IO.Path]::GetTempPath()) "reviewr-install-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    Write-Output "${Say}: downloading $Archive ($Tag)"
    Get-Asset "$Base/$Archive" (Join-Path $Tmp $Archive)
    Get-Asset "$Base/$Checksum" (Join-Path $Tmp $Checksum)

    Write-Output "${Say}: verifying checksum"
    $expected = ((Get-Content -LiteralPath (Join-Path $Tmp $Checksum) -TotalCount 1) -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Tmp $Archive)).Hash.ToLowerInvariant()
    if ($expected -ne $actual) {
        Fail "checksum mismatch (expected $expected, got $actual)"
    }

    # Extract outside bin\, then move the binary in under a sibling name and rename it into
    # place, so an interrupted install never leaves a half-written herdr-reviewr.exe.
    $unpacked = Join-Path $Tmp 'unpacked'
    Expand-Archive -LiteralPath (Join-Path $Tmp $Archive) -DestinationPath $unpacked
    $exe = Join-Path $unpacked "$Name.exe"
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        Fail "$Archive holds no $Name.exe"
    }
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $staged = Join-Path $BinDir "$Name.exe.partial"
    $final = Join-Path $BinDir "$Name.exe"
    Move-Item -LiteralPath $exe -Destination $staged -Force
    Move-Item -LiteralPath $staged -Destination $final -Force
    Write-Output "${Say}: installed $final"
} catch {
    Fail $_.Exception.Message
} finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}
