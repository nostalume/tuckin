param(
    [Parameter(Mandatory)]
    [ValidateSet('prepare', 'verify')]
    [string] $Phase,
    [Parameter(Mandatory)]
    [string] $Root
)

$ErrorActionPreference = 'Stop'
$reader = 'S-1-5-21-91827364-18273645-56473829-1001'
if ($Phase -eq 'prepare') {
    $acl = [IO.Directory]::GetAccessControl($Root)
    $sid = [Security.Principal.SecurityIdentifier]::new($reader)
    $rule = [Security.AccessControl.FileSystemAccessRule]::new(
        $sid, 'ReadData, WriteData', 'ObjectInherit', 'InheritOnly', 'Allow')
    $acl.AddAccessRule($rule)
    [IO.Directory]::SetAccessControl($Root, $acl)
    return
}

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;

public static class TuckinAuthz
{
    [StructLayout(LayoutKind.Sequential)]
    private struct Luid { public uint Low; public int High; }

    [StructLayout(LayoutKind.Sequential)]
    private struct SidAndAttributes { public IntPtr Sid; public uint Attributes; }

    [StructLayout(LayoutKind.Sequential)]
    private struct Request
    {
        public uint DesiredAccess;
        public IntPtr PrincipalSelf, ObjectTypeList;
        public uint ObjectTypeListLength;
        public IntPtr OptionalArguments;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct Reply
    {
        public uint ResultListLength;
        public IntPtr GrantedAccessMask, SaclEvaluationResults, Error;
    }

    [DllImport("authz.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool AuthzInitializeResourceManager(uint flags, IntPtr check, IntPtr compute, IntPtr free, string name, out IntPtr manager);
    [DllImport("authz.dll", SetLastError = true)]
    private static extern bool AuthzInitializeContextFromSid(uint flags, IntPtr sid, IntPtr manager, IntPtr expiration, Luid id, IntPtr args, out IntPtr context);
    [DllImport("authz.dll", SetLastError = true)]
    private static extern bool AuthzAddSidsToContext(IntPtr context, [In] SidAndAttributes[] sids, uint count, IntPtr restricted, uint restrictedCount, out IntPtr extended);
    [DllImport("authz.dll", SetLastError = true)]
    private static extern bool AuthzAccessCheck(uint flags, IntPtr context, ref Request request, IntPtr audit, byte[] descriptor, IntPtr optional, uint optionalCount, ref Reply reply, IntPtr results);
    [DllImport("authz.dll")]
    private static extern bool AuthzFreeContext(IntPtr context);
    [DllImport("authz.dll")]
    private static extern bool AuthzFreeResourceManager(IntPtr manager);

    private static IntPtr AllocateSid(string value)
    {
        var sid = new SecurityIdentifier(value);
        var bytes = new byte[sid.BinaryLength];
        sid.GetBinaryForm(bytes, 0);
        var buffer = Marshal.AllocHGlobal(bytes.Length);
        Marshal.Copy(bytes, 0, buffer, bytes.Length);
        return buffer;
    }

    public static string Evaluate(byte[] descriptor, string user, string[] groups, uint requested)
    {
        IntPtr manager = IntPtr.Zero, context = IntPtr.Zero, extended = IntPtr.Zero;
        IntPtr sid = IntPtr.Zero, result = IntPtr.Zero;
        var sids = new SidAndAttributes[groups.Length];
        try
        {
            if (!AuthzInitializeResourceManager(1, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, null, out manager))
                throw new Win32Exception();
            sid = AllocateSid(user);
            if (!AuthzInitializeContextFromSid(2, sid, manager, IntPtr.Zero, new Luid(), IntPtr.Zero, out context))
                throw new Win32Exception();
            for (int i = 0; i < groups.Length; i++)
                sids[i] = new SidAndAttributes { Sid = AllocateSid(groups[i]), Attributes = 4 };
            if (!AuthzAddSidsToContext(context, sids, (uint)sids.Length, IntPtr.Zero, 0, out extended))
                throw new Win32Exception();
            var request = new Request { DesiredAccess = requested };
            result = Marshal.AllocHGlobal(8);
            var reply = new Reply { ResultListLength = 1, GrantedAccessMask = result, Error = IntPtr.Add(result, 4) };
            if (!AuthzAccessCheck(0, extended, ref request, IntPtr.Zero, descriptor, IntPtr.Zero, 0, ref reply, IntPtr.Zero))
                throw new Win32Exception();
            var error = Marshal.ReadInt32(reply.Error);
            if (error != 0 && error != 5)
                throw new Win32Exception(error);
            return String.Format("{0}:0x{1:x}", error, Marshal.ReadInt32(result));
        }
        finally
        {
            if (extended != IntPtr.Zero) AuthzFreeContext(extended);
            if (context != IntPtr.Zero) AuthzFreeContext(context);
            if (manager != IntPtr.Zero) AuthzFreeResourceManager(manager);
            foreach (var group in sids) Marshal.FreeHGlobal(group.Sid);
            Marshal.FreeHGlobal(sid);
            Marshal.FreeHGlobal(result);
        }
    }
}
'@

$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$groups = @('S-1-1-0', 'S-1-5-11', 'S-1-5-32-545', 'S-1-5-4')
foreach ($name in @('control', 'created', 'replaced', 'repaired')) {
    $path = Join-Path $Root $name
    $descriptor = [IO.File]::GetAccessControl($path).GetSecurityDescriptorBinaryForm()
    $ownerAccess = ([TuckinAuthz]::Evaluate($descriptor, $owner, $groups, 1)) + '/' +
        ([TuckinAuthz]::Evaluate($descriptor, $owner, $groups, 2))
    $readerAccess = ([TuckinAuthz]::Evaluate($descriptor, $reader, $groups, 1)) + '/' +
        ([TuckinAuthz]::Evaluate($descriptor, $reader, $groups, 2))
    Write-Output "$name owner=$ownerAccess reader=$readerAccess"
    $expected = if ($name -eq 'control') { '0:0x1/0:0x2' } else { '5:0x0/5:0x0' }
    if ($ownerAccess -ne '0:0x1/0:0x2' -or $readerAccess -ne $expected) {
        throw "Unexpected read/write access for $name"
    }
}
