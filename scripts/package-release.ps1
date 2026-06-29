$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    cargo build --release

    $sourceExe = Join-Path $repoRoot "target\release\claude-desktop-updater.exe"
    $releaseExe = Join-Path $repoRoot "target\release\Claude Desktop Updater.exe"
    $portableDir = Join-Path $repoRoot "target\release\ClaudeDesktopUpdater"
    $portableExe = Join-Path $portableDir "Claude Desktop Updater.exe"

    if (!(Test-Path -LiteralPath $sourceExe)) {
        throw "Build output not found: $sourceExe"
    }

    New-Item -ItemType Directory -Force -Path $portableDir | Out-Null
    Copy-Item -LiteralPath $sourceExe -Destination $releaseExe -Force
    Copy-Item -LiteralPath $sourceExe -Destination $portableExe -Force

    Remove-Item -LiteralPath $sourceExe -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $repoRoot "target\release\claude-launcher.exe") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $repoRoot "target\release\ClaudeDesktopUpdaterPortable") -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $portableDir "claude-launcher.exe") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $portableDir "claude-desktop-updater-preview.exe") -Force -ErrorAction SilentlyContinue

    & $releaseExe --self-test
    & $portableExe --self-test

    Get-Item -LiteralPath $releaseExe, $portableExe |
        Select-Object FullName, Length, LastWriteTime
}
finally {
    Pop-Location
}
