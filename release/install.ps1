[CmdletBinding()]
param(
    # 一个兼容周期内保留旧目录，避免现有 PATH 失效；目录内同时安装 kernary.exe/harness.exe。
    [string]$DestinationDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\Harness\bin'),
    [switch]$Rollback
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$destination = [System.IO.Path]::GetFullPath($DestinationDirectory)
$root = [System.IO.Path]::GetPathRoot($destination)
if ($destination.TrimEnd('\') -eq $root.TrimEnd('\')) { throw '拒绝把 Kernary 安装到文件系统根目录。' }
$primaryTarget = Join-Path $destination 'kernary.exe'
$compatibilityTarget = Join-Path $destination 'harness.exe'
$rollbackDirectory = Join-Path $destination 'rollback'
New-Item -ItemType Directory -Force -Path $destination | Out-Null
New-Item -ItemType Directory -Force -Path $rollbackDirectory | Out-Null

function Test-InstalledSet([string]$Directory) {
    $primary = Join-Path $Directory 'kernary.exe'
    $compatibility = Join-Path $Directory 'harness.exe'
    if (Test-Path -LiteralPath $primary -PathType Leaf) { & $primary --version | Out-Null }
    if (Test-Path -LiteralPath $compatibility -PathType Leaf) { & $compatibility --version | Out-Null }
    if (-not (Test-Path -LiteralPath $primary -PathType Leaf) -and -not (Test-Path -LiteralPath $compatibility -PathType Leaf)) {
        throw "Binary set 为空：$Directory"
    }
}

function Move-CurrentSet([string]$TargetDirectory) {
    New-Item -ItemType Directory -Force -Path $TargetDirectory | Out-Null
    foreach ($name in @('kernary.exe', 'harness.exe')) {
        $source = Join-Path $destination $name
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Move-Item -LiteralPath $source -Destination (Join-Path $TargetDirectory $name)
        }
    }
}

function Move-SetToDestination([string]$SourceDirectory) {
    foreach ($name in @('kernary.exe', 'harness.exe')) {
        $source = Join-Path $SourceDirectory $name
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Move-Item -LiteralPath $source -Destination (Join-Path $destination $name)
        }
    }
}

if ($Rollback) {
    $previous = Get-ChildItem -LiteralPath $rollbackDirectory -Directory | Sort-Object Name -Descending | Select-Object -First 1
    if ($null -eq $previous) { throw '没有可回滚的 Kernary binary set。' }
    $swap = Join-Path $destination ("rollback-swap-{0}" -f $PID)
    Move-CurrentSet $swap
    try {
        Move-SetToDestination $previous.FullName
        Test-InstalledSet $destination
        $stamp = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        if (@(Get-ChildItem -LiteralPath $swap -File).Count -gt 0) {
            Move-Item -LiteralPath $swap -Destination (Join-Path $rollbackDirectory "set-$stamp-current")
        } else { Remove-Item -LiteralPath $swap -Force }
        if (@(Get-ChildItem -LiteralPath $previous.FullName -Force).Count -eq 0) { Remove-Item -LiteralPath $previous.FullName -Force }
    }
    catch {
        foreach ($target in @($primaryTarget, $compatibilityTarget)) {
            if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Force }
        }
        Move-SetToDestination $swap
        throw
    }
    Write-Host "Kernary rollback complete: $destination"
    exit 0
}

$primarySource = Join-Path $PSScriptRoot 'bin\kernary.exe'
$compatibilitySource = Join-Path $PSScriptRoot 'bin\harness.exe'
foreach ($source in @($primarySource, $compatibilitySource)) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "发布包缺少 $source" }
}
$staging = Join-Path $destination ("install-staging-{0}" -f $PID)
New-Item -ItemType Directory -Force -Path $staging | Out-Null
Copy-Item -LiteralPath $primarySource -Destination (Join-Path $staging 'kernary.exe')
Copy-Item -LiteralPath $compatibilitySource -Destination (Join-Path $staging 'harness.exe')
Test-InstalledSet $staging

$stamp = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$previousSet = Join-Path $rollbackDirectory "set-$stamp"
Move-CurrentSet $previousSet
try {
    Move-SetToDestination $staging
    Test-InstalledSet $destination
    if (@(Get-ChildItem -LiteralPath $staging -Force).Count -eq 0) { Remove-Item -LiteralPath $staging -Force }
    if (@(Get-ChildItem -LiteralPath $previousSet -Force).Count -eq 0) { Remove-Item -LiteralPath $previousSet -Force }
}
catch {
    foreach ($target in @($primaryTarget, $compatibilityTarget)) {
        if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Force }
    }
    Move-SetToDestination $previousSet
    if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    throw
}
Write-Host "Kernary installed: $primaryTarget"
Write-Host "Harness compatibility alias: $compatibilityTarget"
Write-Host "Add to PATH if needed: $destination"
