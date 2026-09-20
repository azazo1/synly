[CmdletBinding()]
param(
    [string]$Binary = "target/release/synly.exe",

    [string]$OutputDir = "dist",

    [string]$Version,

    [string]$Arch,

    [switch]$Fake
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

function Resolve-Arch {
    param([string]$RequestedArch)

    if (-not [string]::IsNullOrWhiteSpace($RequestedArch)) {
        return $RequestedArch
    }
    if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
        return "aarch64"
    }
    return "x86_64"
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
$resolvedArch = Resolve-Arch $Arch
$suffix = ""
if ($Fake -or $env:SYNLY_FAKE_DIST -eq "1" -or $env:SYNLY_FAKE_DIST -eq "true") {
    $suffix = "-fake"
}
$resolvedOutputDir = [IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $resolvedOutputDir | Out-Null
$archive = Join-Path $resolvedOutputDir "synly-$resolvedVersion-windows-$resolvedArch$suffix.zip"
Write-Step "Creating $archive"
$noticesStage = Join-Path $resolvedOutputDir ("audio-notices-" + [Guid]::NewGuid().ToString("N"))
$noticesDir = Join-Path $noticesStage "audio-licenses"
try {
    & (Join-Path $PSScriptRoot "package-audio-notices.ps1") -Destination $noticesDir
    if (Test-Path -LiteralPath $archive -PathType Leaf) {
        Remove-Item -LiteralPath $archive -Force
    }
    $payload = New-Object System.Collections.Generic.List[string]
    [void]$payload.Add($binaryPath)
    [void]$payload.Add($noticesDir)
    $bytes = [IO.File]::ReadAllBytes($binaryPath)
    $ascii = [Text.Encoding]::ASCII.GetString($bytes)
    if ($ascii.Contains("SDL2.dll")) {
        Write-Step "Bundling SDL2.dll"
        $sdlDir = $env:SYNLY_SDL2_DIR
        if ([string]::IsNullOrWhiteSpace($sdlDir)) {
            $vcpkgRoot = $env:VCPKG_ROOT
            if ([string]::IsNullOrWhiteSpace($vcpkgRoot)) {
                $vcpkgRoot = $env:VCPKG_INSTALLATION_ROOT
            }
            if (-not [string]::IsNullOrWhiteSpace($vcpkgRoot)) {
                $sdlDir = Join-Path $vcpkgRoot "installed/x64-windows/bin"
            }
        }
        $dll = $null
        if (-not [string]::IsNullOrWhiteSpace($sdlDir)) {
            $candidate = Join-Path $sdlDir "SDL2.dll"
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { $dll = $candidate }
        }
        $beside = Join-Path ([IO.Path]::GetDirectoryName($binaryPath)) "SDL2.dll"
        if ($null -eq $dll -and (Test-Path -LiteralPath $beside -PathType Leaf)) {
            $dll = $beside
        }
        if ($null -eq $dll) {
            throw "synly.exe 链接了 SDL2.dll, 但找不到运行库. 请设置 SYNLY_SDL2_DIR 或安装 vcpkg sdl2:x64-windows"
        }
        $license = Join-Path (Split-Path -Parent $PSScriptRoot) "licenses/audio/SDL2-LICENSE.txt"
        if (-not (Test-Path -LiteralPath $license -PathType Leaf) -or (Get-Item -LiteralPath $license).Length -eq 0) {
            throw "缺少 SDL2 许可全文: $license"
        }
        Copy-Item -LiteralPath $license -Destination (Join-Path $noticesDir "SDL2-LICENSE.txt")
        [void]$payload.Add($dll)
        $sdl3 = Join-Path ([IO.Path]::GetDirectoryName($dll)) "SDL3.dll"
        $dllText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($dll))
        if ($dllText.Contains("sdl2-compat:")) {
            if (-not (Test-Path -LiteralPath $sdl3 -PathType Leaf)) {
                throw "sdl2-compat 还需要同目录的 SDL3.dll"
            }
            $sdl3License = Join-Path (Split-Path -Parent $PSScriptRoot) "licenses/audio/SDL3-LICENSE.txt"
            if (-not (Test-Path -LiteralPath $sdl3License -PathType Leaf) -or (Get-Item -LiteralPath $sdl3License).Length -eq 0) {
                throw "缺少 SDL3 许可全文: $sdl3License"
            }
            Copy-Item -LiteralPath $sdl3License -Destination (Join-Path $noticesDir "SDL3-LICENSE.txt")
            [void]$payload.Add($sdl3)
        }
    }
    Compress-Archive -LiteralPath @($payload.ToArray()) -DestinationPath $archive -Force
    if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
        throw "Windows distribution archive creation failed: $archive"
    }
} finally {
    # 只清理本次生成的固定文件, 不递归删除输出目录.
    foreach ($name in @("README.md", "sunshine-GPL-3.0.txt", "moonlight-common-GPL-3.0.txt", "moonlight-qt-GPL-3.0.txt", "SDL2-LICENSE.txt", "SDL3-LICENSE.txt")) {
        $file = Join-Path $noticesDir $name
        if (Test-Path -LiteralPath $file -PathType Leaf) { Remove-Item -LiteralPath $file -Force }
    }
    if (Test-Path -LiteralPath $noticesDir -PathType Container) { [IO.Directory]::Delete($noticesDir) }
    if (Test-Path -LiteralPath $noticesStage -PathType Container) { [IO.Directory]::Delete($noticesStage) }
}

Write-Step "Completed $archive"
