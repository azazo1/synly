param(
    [string]$Mode = "auto"
)

$ErrorActionPreference = "Stop"
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptDir
$envFile = Join-Path $repoRoot "secrets/synly-signing.env"
if (Test-Path -LiteralPath $envFile -PathType Leaf) {
    foreach ($line in Get-Content -LiteralPath $envFile) {
        if ($line -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], "Process")
        }
    }
}

if ($Mode -notin @("auto", "debug", "release")) {
    throw "用法: android-build.ps1 [auto|debug|release]"
}

$signingVars = @(
    "SYNLY_ANDROID_KEYSTORE_PASSWORD",
    "SYNLY_ANDROID_KEY_ALIAS",
    "SYNLY_ANDROID_KEY_PASSWORD"
)
$keystoreSource = ""
if (-not [string]::IsNullOrWhiteSpace($env:SYNLY_ANDROID_KEYSTORE_BASE64)) {
    $keystoreSource = "base64"
} elseif (-not [string]::IsNullOrWhiteSpace($env:SYNLY_ANDROID_KEYSTORE_FILE)) {
    $keystoreSource = "file"
}

$missing = @()
foreach ($name in $signingVars) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        $missing += $name
    }
}

if ($Mode -eq "auto") {
    if ($keystoreSource -or $missing.Count -gt 0) {
        $Mode = "release"
    } else {
        $Mode = "debug"
    }
}

if ($Mode -eq "release") {
    if (-not $keystoreSource -or $missing.Count -gt 0) {
        throw "Android 签名配置不完整, 缺少: keystore 或 $($missing -join ', ')"
    }
    if ($keystoreSource -eq "base64") {
        $keystore = & (Join-Path $scriptDir "android-prepare-signing.ps1") "dist/keystore"
        $env:SYNLY_ANDROID_KEYSTORE_FILE = $keystore
    } elseif (-not (Test-Path -LiteralPath $env:SYNLY_ANDROID_KEYSTORE_FILE -PathType Leaf)) {
        throw "Android 签名 keystore 不存在: $env:SYNLY_ANDROID_KEYSTORE_FILE"
    }
    Write-Host "[android] building signed release APK"
    & (Join-Path $scriptDir "android-gradle.ps1") "assembleRelease"
} else {
    Write-Host "[android] building debug APK"
    & (Join-Path $scriptDir "android-gradle.ps1") "assembleDebug"
}
