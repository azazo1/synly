param(
    [string]$GradleTask = '',
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$GradleArgs
)

$ErrorActionPreference = 'Stop'

if (-not $GradleTask) {
    Write-Error '用法: android-gradle.ps1 <GradleTask> [gradle args...]'
    exit 1
}

function Resolve-JavaHome {
    if ($env:JAVA_HOME -and (Test-Path (Join-Path $env:JAVA_HOME 'bin\java.exe'))) {
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $line = "$(& (Join-Path $env:JAVA_HOME 'bin\java.exe') -version 2>&1 | Select-Object -First 1)"
        } finally {
            $ErrorActionPreference = $previous
        }
        $major = 0
        if ($line -match 'version "(\d+)(?:\.|")') {
            $major = [int]$Matches[1]
        }
        if ($major -ge 17 -and $major -le 23) {
            return $env:JAVA_HOME
        }
    }
    $jbrs = @(
        (Join-Path $env:LOCALAPPDATA 'Programs\Android Studio\jbr'),
        'C:\Program Files\Android\Android Studio\jbr'
    )
    foreach ($jbr in $jbrs) {
        if (Test-Path (Join-Path $jbr 'bin\java.exe')) {
            return $jbr
        }
    }
    return $null
}

$env:JAVA_HOME = Resolve-JavaHome
if (-not $env:JAVA_HOME) {
    throw '未找到兼容的 JDK (17-23), 请设置 JAVA_HOME'
}

if (-not $env:ANDROID_HOME) {
    $sdkRoot = Join-Path $env:LOCALAPPDATA 'Android\Sdk'
    if (Test-Path $sdkRoot) {
        $env:ANDROID_HOME = $sdkRoot
    }
}
if (-not $env:ANDROID_HOME -or -not (Test-Path $env:ANDROID_HOME)) {
    throw 'ANDROID_HOME 未设置, 且未在默认位置找到 Android SDK'
}

Set-Location (Join-Path $PSScriptRoot '..\android')
.\gradlew.bat $GradleTask @GradleArgs
