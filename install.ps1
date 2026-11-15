#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$Tag
)

$ErrorActionPreference = 'Stop'
if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -ne 'X64' -or
    [Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'No release binary for this platform. See Build from source.'
}

$repo = if ($env:TUCKIN_REPOSITORY) { $env:TUCKIN_REPOSITORY } else { 'nostalume/tuckin' }
$base = "https://github.com/$repo/releases/download/$Tag"
$archive = 'tuckin-windows-x86_64.zip'
$binDir = if ($env:TUCKIN_INSTALL_DIR) { $env:TUCKIN_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("tuckin-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null

try {
    $zip = Join-Path $scratch $archive
    $sums = Join-Path $scratch 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive" -OutFile $zip
    Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums
    $pattern = '^([0-9a-f]{64})  ' + [regex]::Escape($archive) + '$'
    $hash = Select-String -LiteralPath $sums -Pattern $pattern
    if (@($hash).Count -ne 1 -or ($hash.Matches[0].Groups[1].Value -ne (Get-FileHash -LiteralPath $zip).Hash)) {
        throw "SHA-256 mismatch for $archive"
    }

    Expand-Archive -LiteralPath $zip -DestinationPath $scratch
    $destination = Join-Path (New-Item -ItemType Directory -Path $binDir -Force).FullName 'tuckin.exe'
    Copy-Item -LiteralPath (Join-Path $scratch 'tuckin.exe') -Destination $destination -Force
    Write-Output "Installed $destination"
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
