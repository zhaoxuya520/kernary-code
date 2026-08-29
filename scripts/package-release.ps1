[CmdletBinding()]
param(
    [string]$Target,
    [string]$OutputDirectory = 'dist',
    [switch]$SkipBuild,
    [string]$CargoExecutable,
    [string]$RustcExecutable
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$project = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([string]::IsNullOrWhiteSpace($CargoExecutable)) {
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    $CargoExecutable = if ($null -ne $cargoCommand) {
        $cargoCommand.Source
    } else {
        Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    }
}
if (-not (Test-Path -LiteralPath $CargoExecutable -PathType Leaf)) {
    throw "找不到 Cargo；请通过 -CargoExecutable 提供绝对路径：$CargoExecutable"
}
if ([string]::IsNullOrWhiteSpace($RustcExecutable)) {
    $RustcExecutable = Join-Path (Split-Path -Parent $CargoExecutable) 'rustc.exe'
}
if (-not (Test-Path -LiteralPath $RustcExecutable -PathType Leaf)) {
    throw "找不到 rustc；请通过 -RustcExecutable 提供绝对路径：$RustcExecutable"
}
if ([string]::IsNullOrWhiteSpace($Target)) {
    $Target = ((& $RustcExecutable -vV) | Select-String '^host:\s+(.+)$').Matches[0].Groups[1].Value
}
$metadata = (& $CargoExecutable metadata --no-deps --format-version 1 | ConvertFrom-Json)
$version = ($metadata.packages | Where-Object name -eq 'harness-cli').version
$output = if ([System.IO.Path]::IsPathRooted($OutputDirectory)) {
    [System.IO.Path]::GetFullPath($OutputDirectory)
} else {
    [System.IO.Path]::GetFullPath((Join-Path $project $OutputDirectory))
}
$packageName = "kernary-code-$version-$Target"
$package = Join-Path $output $packageName
$archive = Join-Path $output "$packageName.zip"
if ((Test-Path -LiteralPath $package) -or (Test-Path -LiteralPath $archive)) {
    throw "发布目标已存在，拒绝覆盖：$packageName"
}
New-Item -ItemType Directory -Force -Path (Join-Path $package 'bin') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $package 'completions') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $package 'man') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $package 'assets') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $package 'examples') | Out-Null

if (-not $SkipBuild) {
    & $CargoExecutable build --locked --release -p harness-cli --bins --target $Target
    if ($LASTEXITCODE -ne 0) { throw 'cargo release build failed' }
}
$extension = if ($Target -like '*windows*') { '.exe' } else { '' }
$binary = Join-Path $project "target\$Target\release\kernary$extension"
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "找不到 release binary：$binary"
}
Copy-Item -LiteralPath $binary -Destination (Join-Path $package "bin\kernary$extension")
Copy-Item -LiteralPath $binary -Destination (Join-Path $package "bin\harness$extension")
Copy-Item -LiteralPath (Join-Path $project 'LICENSE-APACHE') -Destination $package
Copy-Item -LiteralPath (Join-Path $project 'Cargo.lock') -Destination (Join-Path $package 'DEPENDENCIES.lock')
Copy-Item -LiteralPath (Join-Path $project 'assets\kernary-kern.svg') -Destination (Join-Path $package 'assets')
Copy-Item -LiteralPath (Join-Path $project 'kernary.providers.example.toml') -Destination (Join-Path $package 'examples\kernary.providers.toml')
Copy-Item -LiteralPath (Join-Path $project 'kernary.lsp.example.toml') -Destination (Join-Path $package 'examples\kernary.lsp.toml')
Copy-Item -LiteralPath (Join-Path $project 'kernary.example.toml') -Destination (Join-Path $package 'examples\kernary.toml')
Copy-Item -LiteralPath (Join-Path $project 'kernary.mcp.example.toml') -Destination (Join-Path $package 'examples\kernary.mcp.toml')
Copy-Item -LiteralPath (Join-Path $project 'kernary.permissions.example.toml') -Destination (Join-Path $package 'examples\kernary.permissions.toml')
Copy-Item -LiteralPath (Join-Path $project 'release\README_ZH.md') -Destination $package
Copy-Item -LiteralPath (Join-Path $project 'release\install.ps1') -Destination $package
Copy-Item -LiteralPath (Join-Path $project 'release\install.sh') -Destination $package

$utf8 = New-Object System.Text.UTF8Encoding($false)
function Write-Generated([string]$Path, [string[]]$Lines) {
    [System.IO.File]::WriteAllText($Path, (($Lines -join "`n") + "`n"), $utf8)
}
Write-Generated (Join-Path $package 'completions\kernary.bash') (& $binary completions bash)
Write-Generated (Join-Path $package 'completions\_kernary') (& $binary completions zsh)
Write-Generated (Join-Path $package 'completions\kernary.fish') (& $binary completions fish)
Write-Generated (Join-Path $package 'completions\_kernary.ps1') (& $binary completions powershell)
Write-Generated (Join-Path $package 'completions\kernary.elv') (& $binary completions elvish)
Write-Generated (Join-Path $package 'man\kernary.1') (& $binary man)
$compatibilityBinary = Join-Path $package "bin\harness$extension"
Write-Generated (Join-Path $package 'completions\harness.bash') (& $compatibilityBinary completions bash)
Write-Generated (Join-Path $package 'completions\_harness') (& $compatibilityBinary completions zsh)
Write-Generated (Join-Path $package 'completions\harness.fish') (& $compatibilityBinary completions fish)
Write-Generated (Join-Path $package 'completions\_harness.ps1') (& $compatibilityBinary completions powershell)
Write-Generated (Join-Path $package 'completions\harness.elv') (& $compatibilityBinary completions elvish)
Write-Generated (Join-Path $package 'man\harness.1') (& $compatibilityBinary man)

$binaryInPackage = Join-Path $package "bin\kernary$extension"
$compatibilityBinaryInPackage = Join-Path $package "bin\harness$extension"
$manifest = [ordered]@{
    schemaVersion = 2
    name = 'kernary-code'
    primaryCommand = "kernary$extension"
    compatibilityCommands = @("harness$extension")
    compatibilityCommand = "harness$extension"
    version = $version
    target = $Target
    primaryBinary = "bin/kernary$extension"
    compatibilityBinaries = @("bin/harness$extension")
    binary = "bin/kernary$extension"
    binaryBytes = (Get-Item -LiteralPath $binaryInPackage).Length
    binarySha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $binaryInPackage).Hash.ToLowerInvariant()
    compatibilityBinarySha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $compatibilityBinaryInPackage).Hash.ToLowerInvariant()
    stateDirectory = '.harness'
    credentialService = 'dev.openai.harness'
    environmentPrefixes = @('KERNARY_', 'HARNESS_')
    dependencyLockSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $package 'DEPENDENCIES.lock')).Hash.ToLowerInvariant()
}
[System.IO.File]::WriteAllText(
    (Join-Path $package 'release-manifest.json'),
    (($manifest | ConvertTo-Json -Depth 4) + "`n"),
    $utf8
)
Compress-Archive -LiteralPath $package -DestinationPath $archive -CompressionLevel Optimal
$archiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
[System.IO.File]::WriteAllText(
    "$archive.sha256",
    "$archiveHash  $([System.IO.Path]::GetFileName($archive))`n",
    $utf8
)
Write-Output $archive
