# herdr `[[build]]` step: download the prebuilt herdr-reviewr binary for this platform from the
# matching GitHub Release into the plugin's bin\ dir. Runs on `herdr plugin install` (a managed
# checkout); `herdr plugin link` skips the build step — for a local checkout, build from source
# with `cargo install --path .`.
#
# The build runs with the plugin checkout as the working directory, so we resolve the plugin root
# from this script's location rather than $env:HERDR_PLUGIN_ROOT (build commands may not receive the
# runtime env). At runtime the pane command reads $env:HERDR_PLUGIN_ROOT\bin\herdr-reviewr.exe

param()

$ErrorActionPreference = 'Stop'

$Name = "herdr-reviewr"
$Repo = "persiyanov/herdr-reviewr"

# Resolve plugin root from this script's location
$Root = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Root "bin"

# Check for supported architecture
$Arch = [System.Environment]::Is64BitOperatingSystem
if (-not $Arch) {
    Write-Error "${Name}: no prebuilt binary for 32-bit Windows — build from source with 'cargo install --path .'" -ErrorAction Stop
    exit 1
}

# The release tag matches the manifest version, so a checkout always pulls its own release.
# Extract version from herdr-plugin.toml using regex
$ManifestPath = Join-Path $Root "herdr-plugin.toml"
try {
    $ManifestContent = Get-Content -Path $ManifestPath -Raw -ErrorAction Stop
} catch {
    Write-Error "${Name}: failed to read manifest at $ManifestPath : $_" -ErrorAction Stop
}

# Match: version = "0.36.2"
if ($ManifestContent -match 'version\s*=\s*"([^"]+)"') {
    $Version = $matches[1]
} else {
    Write-Error "${Name}: could not extract version from $ManifestPath" -ErrorAction Stop
}

$Tag = "v${Version}"

# Windows target triple
$Target = "x86_64-pc-windows-msvc"
$Archive = "${Name}-${Target}.zip"
# taiki-e's checksum sidecar drops the archive extension: <name>-<target>.sha256
$Checksum = "${Name}-${Target}.sha256"
$Base = "https://github.com/${Repo}/releases/download/${Tag}"

# Create temporary directory
$TempDir = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ([System.IO.Path]::GetRandomFileName())) -Force

# Release-asset downloads are eventually-consistent: GitHub's CDN can 404 for a few minutes
# after a release publishes, even though the asset exists. Retry so an install right after a
# release doesn't fail spuriously. Mirrors install.sh's single `dl()` helper, called twice.
function Get-WithRetry {
    param([string]$Url, [string]$OutFile, [string]$Label)
    $MaxRetries = 5
    $RetryDelay = 1  # seconds
    $RetryCount = 0
    while ($RetryCount -lt $MaxRetries) {
        try {
            Invoke-WebRequest -Uri $Url -OutFile $OutFile -ErrorAction Stop
            return
        } catch {
            $RetryCount++
            if ($RetryCount -ge $MaxRetries) {
                Write-Error "${Name}: failed to download $Label after $MaxRetries attempts: $_" -ErrorAction Stop
            }
            Start-Sleep -Seconds $RetryDelay
        }
    }
}

try {
    Write-Host "${Name}: downloading ${Archive} ($Tag)"

    $ArchivePath = Join-Path $TempDir $Archive
    Get-WithRetry -Url "${Base}/${Archive}" -OutFile $ArchivePath -Label $Archive

    $ChecksumPath = Join-Path $TempDir $Checksum
    Get-WithRetry -Url "${Base}/${Checksum}" -OutFile $ChecksumPath -Label $Checksum

    # Verify checksum
    Write-Host "${Name}: verifying checksum"
    $ExpectedChecksum = (Get-Content -Path $ChecksumPath -Raw).Split()[0].Trim()
    $FileHash = Get-FileHash -Path $ArchivePath -Algorithm SHA256
    $ActualChecksum = $FileHash.Hash.ToLower()

    if ($ExpectedChecksum.ToLower() -ne $ActualChecksum) {
        Write-Error "${Name}: checksum mismatch (expected $ExpectedChecksum, got $ActualChecksum)" -ErrorAction Stop
    }

    # Create bin directory if it doesn't exist
    if (-not (Test-Path $BinDir)) {
        New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    }

    # Extract archive
    Expand-Archive -Path $ArchivePath -DestinationPath $TempDir -Force

    # Copy binary to bin directory. The archive contains the actual build artifact filename,
    # which on Windows already carries the .exe extension (unlike the Unix tar.gz, whose
    # member is the bare `herdr-reviewr`).
    $ExtractedBinary = Join-Path $TempDir "${Name}.exe"
    $TargetBinary = Join-Path $BinDir "${Name}.exe"

    if (-not (Test-Path $ExtractedBinary)) {
        Write-Error "${Name}: extracted binary not found at $ExtractedBinary" -ErrorAction Stop
    }

    Copy-Item -Path $ExtractedBinary -Destination $TargetBinary -Force

    Write-Host "${Name}: installed $TargetBinary"

} finally {
    # Clean up temporary directory
    if (Test-Path $TempDir) {
        Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
