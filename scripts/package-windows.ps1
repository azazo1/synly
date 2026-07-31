[CmdletBinding()]
param(
    [string]$Binary = "target/release/synly.exe",

    [string]$OutputDir = "dist",

    [string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Write-Step {
    param([string]$Message)

    Write-Host "[dist] $Message"
}

function Resolve-Version {
    param([string]$RequestedVersion)

    if (-not [string]::IsNullOrWhiteSpace($RequestedVersion)) {
        return $RequestedVersion
    }

    Write-Step "Reading Cargo package version"
    $metadata = & cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to read the Cargo package version"
    }
    $package = @($metadata.packages | Where-Object { $_.name -eq "synly" }) | Select-Object -First 1
    if ($null -eq $package) {
        throw "The Cargo metadata does not contain the synly package"
    }
    return [string]$package.version
}

$binaryPath = [IO.Path]::GetFullPath($Binary)
if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
    throw "Windows executable does not exist: $binaryPath"
}

$header = [IO.File]::ReadAllBytes($binaryPath)
if ($header.Length -lt 2 -or $header[0] -ne 0x4D -or $header[1] -ne 0x5A) {
    throw "File is not a valid Windows PE executable: $binaryPath"
}

$resolvedVersion = Resolve-Version $Version
$resolvedOutputDir = [IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $resolvedOutputDir | Out-Null
$archive = Join-Path $resolvedOutputDir "synly-$resolvedVersion-windows-x86_64.zip"
Write-Step "Creating $archive"
Compress-Archive -LiteralPath $binaryPath -DestinationPath $archive -Force
if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
    throw "Windows distribution archive creation failed: $archive"
}

Write-Step "Completed $archive"
