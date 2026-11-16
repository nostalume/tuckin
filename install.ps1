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

function Add-TuckinPath([string]$Directory) {
    $directoryEntry = $Directory.Replace('/', '\').TrimEnd('\')
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $raw = $key.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $kind = if ($null -eq $raw) { 'String' } else { $key.GetValueKind('Path') }
        $userPath = if ($kind -eq 'ExpandString') { [Environment]::ExpandEnvironmentVariables($raw) } else { $raw }
        if (-not (($userPath -split ';').Replace('/', '\').Trim('"').TrimEnd('\') -contains $directoryEntry)) {
            $next = if ($raw) { "$raw;$Directory" } else { $Directory }
            if (-not ('TuckinInstaller.EnvironmentChange' -as [type])) {
                Add-Type -Namespace TuckinInstaller -Name EnvironmentChange -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr window, uint message,
    System.IntPtr wParam, string lParam, uint flags, uint timeout, out System.IntPtr result);
'@
            }
            $key.SetValue('Path', $next, $kind)
            $result = [IntPtr]::Zero
            [void][TuckinInstaller.EnvironmentChange]::SendMessageTimeout(
                [IntPtr]0xffff, 0x001a, [IntPtr]::Zero, 'Environment', 2, 1000, [ref]$result)
        }
    } finally {
        $key.Dispose()
    }

    if (-not (($env:Path -split ';').Replace('/', '\').Trim('"').TrimEnd('\') -contains $directoryEntry)) {
        $env:Path = if ($env:Path) { "$env:Path;$Directory" } else { $Directory }
    }
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
    if (-not $env:TUCKIN_INSTALL_DIR) {
        Add-TuckinPath ([System.IO.Path]::GetDirectoryName($destination))
    }
    Write-Output "Installed $destination"
    if (-not $env:TUCKIN_INSTALL_DIR) {
        Write-Output 'Run tuckin --version. If you launched a separate PowerShell process, reopen your terminal first.'
    }
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
