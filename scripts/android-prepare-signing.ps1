param(
    [string]$OutputDir = "dist/keystore"
)

$ErrorActionPreference = "Stop"

$required = @(
    "SYNLY_ANDROID_KEYSTORE_BASE64",
    "SYNLY_ANDROID_KEYSTORE_PASSWORD",
    "SYNLY_ANDROID_KEY_ALIAS",
    "SYNLY_ANDROID_KEY_PASSWORD"
)

foreach ($name in $required) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        throw "缺少签名 secret: $name"
    }
}

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$keystore = Join-Path (Resolve-Path $OutputDir) "release.jks"
$encoded = ($env:SYNLY_ANDROID_KEYSTORE_BASE64 -replace '\s+', '')
$bytes = [Convert]::FromBase64String($encoded)
[IO.File]::WriteAllBytes($keystore, $bytes)
if (-not (Test-Path -LiteralPath $keystore -PathType Leaf) -or (Get-Item -LiteralPath $keystore).Length -eq 0) {
    throw "签名 keystore 解码后为空: $keystore"
}

Write-Output $keystore
