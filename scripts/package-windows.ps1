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

function Resolve-Iscc {
    if (-not [string]::IsNullOrWhiteSpace($env:SYNLY_ISCC)) {
        $explicit = [IO.Path]::GetFullPath($env:SYNLY_ISCC)
        if (-not (Test-Path -LiteralPath $explicit -PathType Leaf)) {
            throw "SYNLY_ISCC 指向的文件不存在: $explicit"
        }
        return $explicit
    }
    $fromPath = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($null -ne $fromPath) {
        return $fromPath.Source
    }
    $candidates = New-Object System.Collections.Generic.List[string]
    if (-not [string]::IsNullOrWhiteSpace(${env:ProgramFiles(x86)})) {
        [void]$candidates.Add((Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"))
    }
    if (-not [string]::IsNullOrWhiteSpace($env:ProgramFiles)) {
        [void]$candidates.Add((Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"))
    }
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        [void]$candidates.Add((Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"))
    }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return $candidate
        }
    }
    throw "找不到 Inno Setup 6 的 ISCC.exe, 请先安装 Inno Setup 6.3 或设置 SYNLY_ISCC"
}

function Assert-PeFile {
    param([string]$Path)

    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 2 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) {
        throw "Not a valid Windows PE file: $Path"
    }
}

# 把播放后端需要的 SDL 运行库随包复制进载荷.
function Copy-AudioRuntime {
    param(
        [string]$BinaryPath,
        [string]$PayloadDir,
        [string]$NoticesDir,
        [string]$RepositoryRoot
    )

    $ascii = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($BinaryPath))
    if (-not $ascii.Contains("SDL2.dll")) {
        return
    }
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
    $beside = Join-Path ([IO.Path]::GetDirectoryName($BinaryPath)) "SDL2.dll"
    if ($null -eq $dll -and (Test-Path -LiteralPath $beside -PathType Leaf)) {
        $dll = $beside
    }
    if ($null -eq $dll) {
        throw "synly.exe 链接了 SDL2.dll, 但找不到运行库. 请设置 SYNLY_SDL2_DIR 或安装 vcpkg sdl2:x64-windows"
    }
    $license = Join-Path $RepositoryRoot "licenses/audio/SDL2-LICENSE.txt"
    if (-not (Test-Path -LiteralPath $license -PathType Leaf) -or (Get-Item -LiteralPath $license).Length -eq 0) {
        throw "缺少 SDL2 许可全文: $license"
    }
    Copy-Item -LiteralPath $license -Destination (Join-Path $NoticesDir "SDL2-LICENSE.txt")
    Copy-Item -LiteralPath $dll -Destination (Join-Path $PayloadDir "SDL2.dll")
    $sdl3 = Join-Path ([IO.Path]::GetDirectoryName($dll)) "SDL3.dll"
    $dllText = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($dll))
    if ($dllText.Contains("sdl2-compat:")) {
        if (-not (Test-Path -LiteralPath $sdl3 -PathType Leaf)) {
            throw "sdl2-compat 还需要同目录的 SDL3.dll"
        }
        $sdl3License = Join-Path $RepositoryRoot "licenses/audio/SDL3-LICENSE.txt"
        if (-not (Test-Path -LiteralPath $sdl3License -PathType Leaf) -or (Get-Item -LiteralPath $sdl3License).Length -eq 0) {
            throw "缺少 SDL3 许可全文: $sdl3License"
        }
        Copy-Item -LiteralPath $sdl3License -Destination (Join-Path $NoticesDir "SDL3-LICENSE.txt")
        Copy-Item -LiteralPath $sdl3 -Destination (Join-Path $PayloadDir "SDL3.dll")
    }
}

$binaryPath = [IO.Path]::GetFullPath($Binary)
if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
    throw "Windows executable does not exist: $binaryPath"
}
Assert-PeFile $binaryPath

$resolvedVersion = Resolve-Version $Version
$resolvedArch = Resolve-Arch $Arch
$suffix = ""
if ($Fake -or $env:SYNLY_FAKE_DIST -eq "1" -or $env:SYNLY_FAKE_DIST -eq "true") {
    $suffix = "-fake"
}
$resolvedOutputDir = [IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $resolvedOutputDir | Out-Null
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$iscc = Resolve-Iscc
$installerScript = Join-Path $PSScriptRoot "installer-windows.iss"
$payloadDir = Join-Path $resolvedOutputDir "payload-windows-$resolvedArch$suffix"
$noticesDir = Join-Path $payloadDir "audio-licenses"
$baseName = "synly-$resolvedVersion-windows-$resolvedArch-setup$suffix"
$installer = Join-Path $resolvedOutputDir "$baseName.exe"

try {
    Write-Step "Staging installer payload"
    New-Item -ItemType Directory -Force -Path $payloadDir | Out-Null
    Copy-Item -LiteralPath $binaryPath -Destination (Join-Path $payloadDir "synly.exe") -Force
    # audio-licenses 必须由许可脚本自己创建, 它拒绝写入已存在的目标.
    & (Join-Path $PSScriptRoot "package-audio-notices.ps1") -Destination $noticesDir
    Copy-AudioRuntime -BinaryPath $binaryPath -PayloadDir $payloadDir -NoticesDir $noticesDir -RepositoryRoot $repositoryRoot

    Write-Step "Building $installer"
    if (Test-Path -LiteralPath $installer -PathType Leaf) {
        Remove-Item -LiteralPath $installer -Force
    }
    & $iscc `
        "/DAppVersion=$resolvedVersion" `
        "/DAppArch=$resolvedArch" `
        "/DPayloadDir=$payloadDir" `
        "/DOutputDir=$resolvedOutputDir" `
        "/DOutputBaseName=$baseName" `
        $installerScript
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup 编译失败, 退出码 $LASTEXITCODE"
    }
    if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw "Windows 安装器没有生成: $installer"
    }
    Assert-PeFile $installer
} finally {
    # payloadDir 是本脚本在输出目录下创建的中间目录, 只清理它自己.
    if (Test-Path -LiteralPath $payloadDir -PathType Container) {
        $resolvedPayload = [IO.Path]::GetFullPath($payloadDir)
        $prefix = $resolvedOutputDir.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
        if (-not $resolvedPayload.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "拒绝清理输出目录之外的路径: $resolvedPayload"
        }
        [IO.Directory]::Delete($resolvedPayload, $true)
    }
}

Write-Step "Completed $installer"
