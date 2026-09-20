Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$work = Join-Path $root (".tmp/audio-notices-test-" + [Guid]::NewGuid().ToString("N"))
$files = @("README.md", "sunshine-GPL-3.0.txt", "moonlight-common-GPL-3.0.txt", "moonlight-qt-GPL-3.0.txt")
New-Item -ItemType Directory -Path $work -Force | Out-Null
function Assert-SameFile([string]$First, [string]$Second) {
    if ((Get-FileHash -LiteralPath $First -Algorithm SHA256).Hash -ne (Get-FileHash -LiteralPath $Second -Algorithm SHA256).Hash) {
        throw "文件内容不一致: $First / $Second"
    }
}
function Expect-Failure([scriptblock]$Action) {
    $failed = $false
    try { & $Action } catch { $failed = $true; Write-Host "按预期拒绝不完整输入" }
    if (-not $failed) { throw "预期失败但操作成功" }
}
Write-Host "[1/3] 校验固定上游许可与逐字复制"
$copyScript = Join-Path $root "scripts/package-audio-notices.ps1"
$notices = Join-Path $work "audio-licenses"
& $copyScript -Destination $notices
foreach ($name in $files) { Assert-SameFile (Join-Path $root "licenses/audio/$name") (Join-Path $notices $name) }
$expected = @{
    "sunshine-GPL-3.0.txt" = "3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986"
    "moonlight-common-GPL-3.0.txt" = "589ed823e9a84c56feb95ac58e7cf384626b9cbf4fda2a907bc36e103de1bad2"
    "moonlight-qt-GPL-3.0.txt" = "589ed823e9a84c56feb95ac58e7cf384626b9cbf4fda2a907bc36e103de1bad2"
}
foreach ($name in $expected.Keys) {
    if ((Get-FileHash -LiteralPath (Join-Path $notices $name) -Algorithm SHA256).Hash -ne $expected[$name]) {
        throw "上游许可全文哈希不匹配: $name"
    }
}
Write-Host "[2/3] 缺失, 空文件和旧目标必须失败"
$repo = Join-Path $work "repo"
$fixtureScripts = Join-Path $repo "scripts"
$fixtureLicenses = Join-Path $repo "licenses/audio"
New-Item -ItemType Directory -Path $fixtureScripts, $fixtureLicenses -Force | Out-Null
Copy-Item -LiteralPath $copyScript -Destination $fixtureScripts
foreach ($name in $files) { Copy-Item -LiteralPath (Join-Path $notices $name) -Destination $fixtureLicenses }
$fixtureCopy = Join-Path $fixtureScripts "package-audio-notices.ps1"
Expect-Failure { & $copyScript -Destination $notices }
foreach ($name in $files) {
    $source = Join-Path $fixtureLicenses $name
    Remove-Item -LiteralPath $source
    $missing = Join-Path $work "missing-$name"
    Expect-Failure { & $fixtureCopy -Destination $missing }
    if (Test-Path -LiteralPath $missing) { throw "缺失输入时不应生成目标" }
    [IO.File]::WriteAllBytes($source, [byte[]]@())
    $empty = Join-Path $work "empty-$name"
    Expect-Failure { & $fixtureCopy -Destination $empty }
    if (Test-Path -LiteralPath $empty) { throw "空输入时不应生成目标" }
    Copy-Item -LiteralPath (Join-Path $notices $name) -Destination $source -Force
}
Write-Host "[3/3] 执行 Windows 安装器打包并静默安装校验, 不运行占位二进制"
Copy-Item -LiteralPath (Join-Path $root "scripts/package-windows.ps1") -Destination $fixtureScripts
Copy-Item -LiteralPath (Join-Path $root "scripts/installer-windows.iss") -Destination $fixtureScripts
$fixtureAssets = Join-Path $repo "assets/windows"
New-Item -ItemType Directory -Path $fixtureAssets -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $root "assets/windows/synly.ico") -Destination $fixtureAssets
$binary = Join-Path $repo "synly.exe"
# 仅满足现有打包脚本的 MZ 标志检查, 不编译或运行任何应用.
[IO.File]::WriteAllBytes($binary, [byte[]]@(0x4D, 0x5A))
$output = Join-Path $work "dist with spaces"
& (Join-Path $fixtureScripts "package-windows.ps1") -Binary $binary -OutputDir $output -Version "test-only" -Arch "x86_64"
$installer = Join-Path $output "synly-test-only-windows-x86_64-setup.exe"
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { throw "安装器未生成: $installer" }
$target = Join-Path $work "installed"
& $installer '/SP-' '/VERYSILENT' '/SUPPRESSMSGBOXES' '/NORESTART' '/NOICONS' "/DIR=$target"
if (-not (Test-Path -LiteralPath (Join-Path $target "synly.exe") -PathType Leaf)) { throw "安装目录缺少主程序" }
Assert-SameFile $binary (Join-Path $target "synly.exe")
foreach ($name in $files) { Assert-SameFile (Join-Path $notices $name) (Join-Path $target "audio-licenses/$name") }
& (Join-Path $target "unins000.exe") '/VERYSILENT' '/SUPPRESSMSGBOXES' '/NORESTART'
if (Test-Path -LiteralPath (Join-Path $target "synly.exe")) { throw "卸载后主程序仍在" }
if (@(Get-ChildItem -LiteralPath $output -Directory).Count -ne 0) { throw "载荷暂存目录未清理" }
Write-Host "音频许可随附测试通过, 测试产物保留于 $work"
