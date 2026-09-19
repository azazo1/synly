[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$sourceDir = Join-Path $root "licenses/audio"
$files = @("README.md", "sunshine-GPL-3.0.txt", "moonlight-common-GPL-3.0.txt", "moonlight-qt-GPL-3.0.txt")

# 先验证所有输入, 防止缺许可时留下看似完整的目标目录.
foreach ($name in $files) {
    $source = Join-Path $sourceDir $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "缺少音频许可文件: $source"
    }
    $item = Get-Item -LiteralPath $source
    if ($item.Length -eq 0 -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "音频许可文件为空或不是普通文件: $source"
    }
}
if (Test-Path -LiteralPath $Destination) {
    throw "音频许可目标必须为新目录: $Destination"
}
Write-Host "[package] 随附音频来源说明和 GPL 全文"
New-Item -ItemType Directory -Path $Destination -Force | Out-Null
foreach ($name in $files) {
    $source = Join-Path $sourceDir $name
    $target = Join-Path $Destination $name
    Copy-Item -LiteralPath $source -Destination $target
    if ((Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ne (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash) {
        throw "音频许可复制校验失败: $target"
    }
}
