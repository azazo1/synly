# 计算应嵌入二进制的构建版本号, 打印到 stdout.
# 已设置 SYNLY_BUILD_VERSION 时原样输出; 否则按 git describe 生成.
# 精确 tag 输出该 tag; 非 tag 追加 - 和 7 位短 hash; 脏工作区改用 ^.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if (-not [string]::IsNullOrWhiteSpace($env:SYNLY_BUILD_VERSION)) {
    Write-Output $env:SYNLY_BUILD_VERSION.Trim()
    exit 0
}

$repoRoot = $null
try {
    $repoRoot = (& git rev-parse --show-toplevel 2>$null | Select-Object -First 1)
} catch {
    $repoRoot = $null
}

$fallback = "unknown"
try {
    $pkgid = & cargo pkgid -p synly 2>$null | Select-Object -First 1
    if ($pkgid -match '@(.+)$') {
        $fallback = $Matches[1]
    }
} catch {
}

if ([string]::IsNullOrWhiteSpace($repoRoot)) {
    Write-Output $fallback
    exit 0
}

$describe = (& git -C $repoRoot describe --tags --always --abbrev=7 2>$null | Select-Object -First 1)
$head = (& git -C $repoRoot rev-parse --short=7 HEAD 2>$null | Select-Object -First 1)
$dirty = -not [string]::IsNullOrWhiteSpace((& git -C $repoRoot status --porcelain 2>$null | Out-String).Trim())
$separator = if ($dirty) { "^" } else { "-" }

if ([string]::IsNullOrWhiteSpace($describe)) {
    Write-Output $fallback
    exit 0
}

if ($describe -match '^(.+)-([0-9]+)-g([0-9a-fA-F]{7,})$') {
    Write-Output ($Matches[1] + $separator + $Matches[3])
    exit 0
}

if ($describe -match '^[0-9a-fA-F]{7,}$') {
    $hash = $describe.Substring(0, [Math]::Min(7, $describe.Length))
    Write-Output ($fallback + $separator + $hash)
    exit 0
}

if ($dirty -and -not [string]::IsNullOrWhiteSpace($head)) {
    Write-Output ($describe + "^" + $head)
    exit 0
}

Write-Output $describe
