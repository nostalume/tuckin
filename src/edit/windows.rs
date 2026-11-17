//! A private editing directory with inheritable access for editor-created files.

use std::borrow::Cow;
use std::ffi::{OsString, c_void};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, IsValidAcl,
    OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_SHARE_READ, FILE_SHARE_WRITE,
    READ_CONTROL,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: the allocation came from a Windows API that requires LocalFree.
        unsafe { LocalFree(self.0) };
    }
}

fn descriptor() -> io::Result<LocalAllocation> {
    let mut token = null_mut();
    // SAFETY: the pseudo-handle is valid and token is a writable out parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned an owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    const USER_WORDS: usize =
        (size_of::<TOKEN_USER>() + SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<usize>());
    let mut buffer = [0usize; USER_WORDS];
    let mut written = 0;
    // SAFETY: the buffer is aligned and large enough for TokenUser and its SID.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size_of::<[usize; USER_WORDS]>() as u32,
            &mut written,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: TokenUser initialized a header and SID in the live buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = null_mut();
    // SAFETY: the SID is live and text is a writable out parameter.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _text = LocalAllocation(text.cast());
    // SAFETY: the conversion returned a NUL-terminated UTF-16 string.
    let len = (0..).find(|&index| unsafe { *text.add(index) } == 0).unwrap();
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    let rule: Vec<u16> =
        format!("O:{sid}D:P(A;OICI;FA;;;{sid})").encode_utf16().chain([0]).collect();
    let mut descriptor = null_mut();
    // SAFETY: rule is terminated and descriptor is a writable out parameter.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            rule.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

/// The directory we hold must carry the owner and single inheritable ACE we requested.
fn verify_directory(directory: &File, expected: &LocalAllocation) -> io::Result<()> {
    let mut actual = null_mut();
    let mut owner = null_mut();
    let mut acl = null_mut();
    // SAFETY: the handle is live and the out parameters receive pointers into actual.
    let result = unsafe {
        GetSecurityInfo(
            directory.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut actual,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let actual = LocalAllocation(actual);
    let mut expected_owner = null_mut();
    let mut expected_acl = null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: both descriptors are live, and all outputs point to initialized storage.
    let valid = unsafe {
        GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) != 0
            && GetSecurityDescriptorDacl(
                expected.0,
                &mut present,
                &mut expected_acl,
                &mut defaulted,
            ) != 0
            && present != 0
            && GetSecurityDescriptorControl(actual.0, &mut control, &mut revision) != 0
            && control & SE_DACL_PROTECTED != 0
            && !owner.is_null()
            && !expected_owner.is_null()
            && EqualSid(owner, expected_owner) != 0
            && same_ace(acl, expected_acl)
    };
    if !valid {
        return Err(io::Error::other(
            "the private directory does not have the requested permissions",
        ));
    }
    Ok(())
}

/// Both ACLs must be borrowed from live Windows security descriptors.
unsafe fn same_ace(left: *const ACL, right: *const ACL) -> bool {
    // SAFETY: descriptor-owned ACLs can be validated and dereferenced while live.
    unsafe {
        if left.is_null()
            || right.is_null()
            || IsValidAcl(left) == 0
            || IsValidAcl(right) == 0
            || (*left).AceCount != 1
            || (*right).AceCount != 1
        {
            return false;
        }
        let mut a = null_mut();
        let mut b = null_mut();
        if GetAce(left, 0, &mut a) == 0 || GetAce(right, 0, &mut b) == 0 {
            return false;
        }
        let size = (*a.cast::<ACE_HEADER>()).AceSize as usize;
        size == (*b.cast::<ACE_HEADER>()).AceSize as usize
            && std::slice::from_raw_parts(a.cast::<u8>(), size)
                == std::slice::from_raw_parts(b.cast::<u8>(), size)
    }
}

/// Give editors a conventional drive path when it resolves to the same file.
pub(super) fn editor_path(path: &Path) -> Cow<'_, Path> {
    if matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
    ) {
        let wide: Vec<_> = path.as_os_str().encode_wide().collect();
        let standard = PathBuf::from(OsString::from_wide(&wide[4..]));
        if standard.canonicalize().ok().as_deref() == Some(path) {
            return Cow::Owned(standard);
        }
    }
    Cow::Borrowed(path)
}

pub(super) struct Session {
    path: PathBuf,
    // Keep the session name in place while an editor can still write to it.
    directory: Option<File>,
}

impl Session {
    /// The caller has admitted this volume for private publication.
    pub(super) fn create(path: PathBuf) -> io::Result<Self> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: wide is terminated and attributes borrows a live security descriptor.
        if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = OpenOptions::new()
            .access_mode(FILE_LIST_DIRECTORY | READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)?;
        let metadata = directory.metadata()?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("the private directory changed during creation"));
        }
        verify_directory(&directory, &descriptor)?;
        Ok(Self { path, directory: Some(directory) })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn close(mut self) -> io::Result<()> {
        self.remove()
    }

    fn remove(&mut self) -> io::Result<()> {
        let Some(directory) = self.directory.take() else { return Ok(()) };
        drop(directory);
        fs::remove_dir_all(&self.path)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}
