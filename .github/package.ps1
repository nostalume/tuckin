#Requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$Tag,
    [Parameter(Mandatory)]
    [ValidateSet('windows-x86_64', 'linux-x86_64', 'macos-aarch64')]
    [string]$Platform
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$windows = $Platform -eq 'windows-x86_64'
$binary = if ($windows) { 'tuckin.exe' } else { 'tuckin' }
$extension = if ($windows) { 'zip' } else { 'tar.gz' }
$archive = Join-Path (New-Item -ItemType Directory -Path dist -Force).FullName "tuckin-$Platform.$extension"
$scratch = [System.IO.Directory]::CreateTempSubdirectory('tuckin-package-').FullName
$source = Join-Path $scratch 'source'
$consumer = Join-Path $scratch 'consumer'

try {
    New-Item -ItemType Directory -Path $source, $consumer | Out-Null
    Copy-Item -LiteralPath "target/release/$binary", 'LICENSE' -Destination $source
    if ($windows) {
        Compress-Archive -LiteralPath "$source/$binary", "$source/LICENSE" -DestinationPath $archive -Force
        Expand-Archive -LiteralPath $archive -DestinationPath $consumer
    } else {
        tar -czf $archive -C $source $binary LICENSE
        tar -xzf $archive -C $consumer
    }

    $exe = Join-Path $consumer $binary
    if ((& $exe --version) -ne "tuckin $($Tag.Substring(1))") {
        throw "The packaged version must match $Tag"
    }

    $repo = (New-Item -ItemType Directory -Path "$scratch/repo").FullName
    $target = (Join-Path $scratch 'target').Replace('\', '/')
    $mapping = ('abs:' + $target) | ConvertTo-Json -Compress
    Push-Location -LiteralPath $repo
    try {
        & $exe init
        @"
[map.home]
posix = $mapping
win = $mapping
"@ | Set-Content -LiteralPath 'tuckin.toml' -Encoding utf8
        New-Item -ItemType Directory -Path 'home' | Out-Null
        Set-Content -LiteralPath 'home/example' -Value 'release smoke test' -Encoding utf8
        & $exe apply --no-scripts
        if ((Get-Content -LiteralPath "$target/example" -Raw).Trim() -ne 'release smoke test') {
            throw 'The packaged executable did not deploy the expected file'
        }
        & $exe diff
    } finally {
        Pop-Location
    }
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
