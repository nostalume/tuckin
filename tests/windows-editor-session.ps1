param([Parameter(Mandatory)][string] $File)

$ErrorActionPreference = 'Stop'
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$directory = [IO.Path]::GetDirectoryName($File)
$directoryAcl = [IO.Directory]::GetAccessControl($directory)
$rules = @($directoryAcl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
if (-not $directoryAcl.AreAccessRulesProtected -or $rules.Count -ne 1 -or
    $rules[0].IdentityReference.Value -ne $owner -or
    $rules[0].InheritanceFlags -ne 'ContainerInherit, ObjectInherit') {
    throw 'The editor directory is not owner-only and inheritable'
}

function Assert-Private([string] $Path) {
    $acl = [IO.File]::GetAccessControl($Path)
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    if ($rules.Count -ne 1 -or $rules[0].IdentityReference.Value -ne $owner) {
        throw 'An editor file grants access to a different account'
    }
}

Assert-Private $File
try {
    [IO.Directory]::Move($directory, ($directory + '-moved'))
    [IO.Directory]::Move(($directory + '-moved'), $directory)
    throw 'The editor directory could be renamed during the session'
} catch [IO.IOException] {
    # The session pins its directory while the editor is live.
}
if ([IO.File]::ReadAllText($File) -ne '{{#seal}}first{{/seal}}') {
    throw 'The editor did not receive the decrypted draft'
}
$replacement = Join-Path $directory 'replacement'
[IO.File]::WriteAllText($replacement, '{{#seal}}second{{/seal}}')
Assert-Private $replacement
[IO.File]::Delete($File)
[IO.File]::Move($replacement, $File)
