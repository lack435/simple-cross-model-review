//! Windows security descriptors, for locking a credential directory or the authorization store to
//! the current user.
//!
//! Everything under `%LOCALAPPDATA%\cross-review` that gates a review — the allowlist store, a
//! provisioned profile home — must be readable and writable by **only** the current user (plus the
//! machine's SYSTEM and Administrators, per Windows norms), with inheritance removed so a permissive
//! ACL on a parent directory cannot widen it. An attacker who could write the store could authorize
//! themselves; an attacker who could read a profile home could steal a reviewer credential. So this
//! module builds one restrictive DACL and both *applies* and *verifies* it **through a handle**
//! (`SetSecurityInfo`/`GetSecurityInfo`, the `HANDLE` forms — never the by-path `…NamedSecurityInfo`),
//! so the object that receives the DACL is provably the object we opened, with no path-reopen TOCTOU
//! in between (impl plan `[f15]`, `[f22]`).
//!
//! This is new FFI in the style of [`crate::winjob`] (which is job objects, not ACLs). It leans on
//! `std::os::windows::io::OwnedHandle` for handle lifetime, and declares the `advapi32` surface it
//! needs directly rather than adding a crate — the serde-only footprint is a project constraint.

#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

type Handle = *mut c_void;
type Bool = i32;
type Dword = u32;

// --- kernel32 -------------------------------------------------------------------------------------

const INVALID_HANDLE_VALUE: isize = -1;

// CreateFileW dwDesiredAccess / share / disposition / flags.
const GENERIC_READ: Dword = 0x8000_0000;
const GENERIC_WRITE: Dword = 0x4000_0000;
const WRITE_DAC: Dword = 0x0004_0000;
const READ_CONTROL: Dword = 0x0002_0000;
// Share reads with other openers but never DELETE: a directory or file opened without
// FILE_SHARE_DELETE cannot be renamed or deleted out from under the held handle ([f20]).
const FILE_SHARE_READ: Dword = 0x0000_0001;
const FILE_SHARE_WRITE: Dword = 0x0000_0002;
const OPEN_EXISTING: Dword = 3;
const CREATE_NEW: Dword = 1;
const FILE_FLAG_BACKUP_SEMANTICS: Dword = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: Dword = 0x0020_0000;
const FILE_ATTRIBUTE_NORMAL: Dword = 0x0000_0080;
const FILE_ATTRIBUTE_REPARSE_POINT: Dword = 0x0000_0400;

#[repr(C)]
struct ByHandleFileInformation {
    file_attributes: Dword,
    creation_time: [Dword; 2],
    last_access_time: [Dword; 2],
    last_write_time: [Dword; 2],
    volume_serial_number: Dword,
    file_size_high: Dword,
    file_size_low: Dword,
    number_of_links: Dword,
    file_index_high: Dword,
    file_index_low: Dword,
}

extern "system" {
    fn CreateFileW(
        file_name: *const u16,
        desired_access: Dword,
        share_mode: Dword,
        security_attributes: *mut c_void,
        creation_disposition: Dword,
        flags_and_attributes: Dword,
        template_file: Handle,
    ) -> Handle;
    fn GetFileInformationByHandle(file: Handle, info: *mut ByHandleFileInformation) -> Bool;
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
    fn GetCurrentProcess() -> Handle;
}

// --- advapi32 -------------------------------------------------------------------------------------

const TOKEN_QUERY: Dword = 0x0008;
// TOKEN_INFORMATION_CLASS::TokenUser
const TOKEN_USER_CLASS: Dword = 1;
// WELL_KNOWN_SID_TYPE
const WIN_LOCAL_SYSTEM_SID: Dword = 22;
const WIN_BUILTIN_ADMINISTRATORS_SID: Dword = 26;
const ACL_REVISION: Dword = 2;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
// SE_OBJECT_TYPE::SE_FILE_OBJECT
const SE_FILE_OBJECT: Dword = 1;
const DACL_SECURITY_INFORMATION: Dword = 0x0000_0004;
const PROTECTED_DACL_SECURITY_INFORMATION: Dword = 0x8000_0000;
// The rights a full owner needs over its own credential file/dir. Standard "full control".
const FILE_ALL_ACCESS: Dword = 0x001F_01FF;
// ACL_INFORMATION_CLASS::AclSizeInformation
const ACL_SIZE_INFORMATION: Dword = 2;
// SECURITY_DESCRIPTOR_CONTROL bit: the DACL is protected from inheritance.
const SE_DACL_PROTECTED: u16 = 0x1000;
const ERROR_SUCCESS: Dword = 0;
// A well-known SID never exceeds this many bytes (SECURITY_MAX_SID_SIZE).
const SECURITY_MAX_SID_SIZE: usize = 68;

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: Dword,
}

#[repr(C)]
struct TokenUser {
    user: SidAndAttributes,
}

#[repr(C)]
struct AclHeader {
    acl_revision: u8,
    sbz1: u8,
    acl_size: u16,
    ace_count: u16,
    sbz2: u16,
}

#[repr(C)]
struct AceHeader {
    ace_type: u8,
    ace_flags: u8,
    ace_size: u16,
}

#[repr(C)]
struct AccessAllowedAce {
    header: AceHeader,
    mask: Dword,
    // The SID begins here; the struct's declared size counts one DWORD of it.
    sid_start: Dword,
}

#[repr(C)]
struct AclSizeInformation {
    ace_count: Dword,
    acl_bytes_in_use: Dword,
    acl_bytes_free: Dword,
}

// The security-descriptor, SID, and ACL APIs live in advapi32; kernel32 (the block above) is linked
// by default but advapi32 must be named explicitly.
#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(process: Handle, desired_access: Dword, token: *mut Handle) -> Bool;
    fn GetTokenInformation(
        token: Handle,
        info_class: Dword,
        info: *mut c_void,
        info_len: Dword,
        return_len: *mut Dword,
    ) -> Bool;
    fn CreateWellKnownSid(
        sid_type: Dword,
        domain_sid: *mut c_void,
        sid: *mut c_void,
        cb_sid: *mut Dword,
    ) -> Bool;
    fn GetLengthSid(sid: *mut c_void) -> Dword;
    fn IsValidSid(sid: *mut c_void) -> Bool;
    fn EqualSid(sid1: *mut c_void, sid2: *mut c_void) -> Bool;
    fn InitializeAcl(acl: *mut c_void, acl_length: Dword, acl_revision: Dword) -> Bool;
    fn AddAccessAllowedAce(
        acl: *mut c_void,
        ace_revision: Dword,
        access_mask: Dword,
        sid: *mut c_void,
    ) -> Bool;
    fn GetAclInformation(
        acl: *mut c_void,
        info: *mut c_void,
        info_len: Dword,
        info_class: Dword,
    ) -> Bool;
    fn GetAce(acl: *mut c_void, ace_index: Dword, ace: *mut *mut c_void) -> Bool;
    fn SetSecurityInfo(
        handle: Handle,
        object_type: Dword,
        security_info: Dword,
        owner: *mut c_void,
        group: *mut c_void,
        dacl: *mut c_void,
        sacl: *mut c_void,
    ) -> Dword;
    fn GetSecurityInfo(
        handle: Handle,
        object_type: Dword,
        security_info: Dword,
        owner: *mut *mut c_void,
        group: *mut *mut c_void,
        dacl: *mut *mut c_void,
        sacl: *mut *mut c_void,
        security_descriptor: *mut *mut c_void,
    ) -> Dword;
    fn GetSecurityDescriptorControl(
        security_descriptor: *mut c_void,
        control: *mut u16,
        revision: *mut Dword,
    ) -> Bool;
}

/// A self-contained SID as raw bytes. A SID is a position-independent blob, so a byte copy is a
/// valid SID whose pointer is `bytes.as_ptr()` — this is how the three principals are carried
/// around after being read out of a token or synthesised from a well-known type.
struct Sid {
    bytes: Vec<u8>,
}

impl Sid {
    fn as_ptr(&self) -> *mut c_void {
        self.bytes.as_ptr() as *mut c_void
    }
}

/// The current process user's SID, copied out of the process token.
fn current_user_sid() -> io::Result<Sid> {
    let mut token: Handle = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle (need not be closed); OpenProcessToken
    // writes the opened token handle into our local `token`.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // OwnedHandle closes the token on drop.
    // SAFETY: `token` is a valid, owned handle returned by OpenProcessToken.
    let _token = unsafe { OwnedHandle::from_raw_handle(token as *mut _) };

    // Two-call idiom: first learn the size, then fill a buffer of that size.
    let mut needed: Dword = 0;
    // SAFETY: a null buffer with zero length asks only for the required size in `needed`.
    unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: buffer is `needed` bytes; the call fills it with a TOKEN_USER whose Sid points inside.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the buffer holds a TOKEN_USER at offset 0.
    let token_user = unsafe { &*(buf.as_ptr() as *const TokenUser) };
    let psid = token_user.user.sid;
    if psid.is_null() {
        return Err(io::Error::other("process token had no user SID"));
    }
    copy_sid(psid)
}

/// A well-known SID (SYSTEM, Administrators) synthesised into an owned byte buffer.
fn well_known_sid(kind: Dword) -> io::Result<Sid> {
    let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE];
    let mut len = SECURITY_MAX_SID_SIZE as Dword;
    // SAFETY: buffer is SECURITY_MAX_SID_SIZE bytes, the documented maximum for any well-known SID.
    let ok = unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(len as usize);
    Ok(Sid { bytes: buf })
}

/// Copy a SID pointed to by `psid` into an owned buffer.
fn copy_sid(psid: *mut c_void) -> io::Result<Sid> {
    // SAFETY: `psid` is expected to point at a valid SID; IsValidSid tolerates any pointer safely
    // enough for a validity check, and we only read `GetLengthSid` bytes after it passes.
    if unsafe { IsValidSid(psid) } == 0 {
        return Err(io::Error::other("SID is not valid"));
    }
    // SAFETY: psid is a valid SID per the check above.
    let len = unsafe { GetLengthSid(psid) } as usize;
    if len == 0 {
        return Err(io::Error::other("SID has zero length"));
    }
    let mut bytes = vec![0u8; len];
    // SAFETY: copying `len` bytes (the SID's own reported length) from a valid SID into our buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(psid as *const u8, bytes.as_mut_ptr(), len);
    }
    Ok(Sid { bytes })
}

/// The three principals every cross-review credential object grants, and nobody else: the current
/// user, plus SYSTEM and Administrators per Windows norms (a machine admin can already read anything;
/// denying them buys nothing and breaks backup/AV).
fn principals() -> io::Result<Vec<Sid>> {
    Ok(vec![
        current_user_sid()?,
        well_known_sid(WIN_LOCAL_SYSTEM_SID)?,
        well_known_sid(WIN_BUILTIN_ADMINISTRATORS_SID)?,
    ])
}

/// Wide, NUL-terminated form of a path for the `…W` APIs.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Open an existing directory with a no-follow handle suitable for reading or setting its security,
/// and reject it if it is a reparse point (a junction/symlink standing in for the real directory).
///
/// `writable` requests `WRITE_DAC` (to *set* the DACL) on top of the read rights; a verify-only open
/// needs only `READ_CONTROL`. The share mode omits `FILE_SHARE_DELETE`, so while this handle is held
/// the directory cannot be renamed or deleted ([f20]).
pub fn open_dir_no_follow(path: &Path, writable: bool) -> io::Result<OwnedHandle> {
    open_no_follow(path, writable, true, OPEN_EXISTING)
}

/// Open (or create, with `CREATE_NEW`) a file with a no-follow handle, rejecting a reparse point.
fn open_file(path: &Path, access: Dword, disposition: Dword) -> io::Result<OwnedHandle> {
    let wide = wide(path);
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path; all other arguments are plain scalars.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            disposition,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    finish_open(raw)
}

fn open_no_follow(
    path: &Path,
    writable: bool,
    is_dir: bool,
    disposition: Dword,
) -> io::Result<OwnedHandle> {
    let wide = wide(path);
    let mut access = READ_CONTROL;
    if writable {
        access |= WRITE_DAC;
    }
    let mut flags = FILE_FLAG_OPEN_REPARSE_POINT;
    if is_dir {
        // Required to obtain a handle to a *directory* rather than a file.
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
    } else {
        flags |= FILE_ATTRIBUTE_NORMAL;
    }
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path; the remaining arguments are scalars.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            disposition,
            flags,
            std::ptr::null_mut(),
        )
    };
    finish_open(raw)
}

fn finish_open(raw: Handle) -> io::Result<OwnedHandle> {
    if raw as isize == INVALID_HANDLE_VALUE || raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid, owned handle from CreateFileW; OwnedHandle closes it exactly once on drop.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as *mut _) };
    if is_reparse_point(&handle)? {
        return Err(io::Error::other(
            "path is a reparse point (junction/symlink); refusing to treat it as a real object",
        ));
    }
    Ok(handle)
}

fn is_reparse_point(handle: &OwnedHandle) -> io::Result<bool> {
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is valid; `info` is a correctly sized output struct.
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle() as Handle, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Build the restrictive DACL for `sids` (each granted `FILE_ALL_ACCESS`) as an owned, DWORD-aligned
/// buffer. Allocated as `Vec<u32>` so the `ACL` header's alignment requirement is met.
fn build_dacl(sids: &[Sid]) -> io::Result<Vec<u32>> {
    // ACL header + one ACCESS_ALLOWED_ACE per SID. Each ACE's declared size counts one DWORD of the
    // SID (the `sid_start` field), so the variable part is `GetLengthSid(sid) - 4`.
    let mut size = std::mem::size_of::<AclHeader>();
    for sid in sids {
        let sid_len = sid.bytes.len();
        size += std::mem::size_of::<AccessAllowedAce>() - std::mem::size_of::<Dword>() + sid_len;
    }
    // Round up to whole u32 words for the backing allocation; the ACL itself is `size` bytes.
    let words = size.div_ceil(std::mem::size_of::<u32>());
    let mut acl = vec![0u32; words];
    let acl_ptr = acl.as_mut_ptr() as *mut c_void;
    // SAFETY: `acl_ptr` addresses `size` writable bytes, DWORD-aligned; size fits a u32 ACL length.
    let ok = unsafe { InitializeAcl(acl_ptr, size as Dword, ACL_REVISION) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    for sid in sids {
        // SAFETY: `acl_ptr` is an initialized ACL with room reserved above; `sid` is a valid SID.
        let ok =
            unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, FILE_ALL_ACCESS, sid.as_ptr()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(acl)
}

/// Apply the restrictive, inheritance-protected DACL to an open handle (a directory or file), so the
/// object is accessible to only the current user, SYSTEM, and Administrators.
///
/// `PROTECTED_DACL_SECURITY_INFORMATION` strips inherited ACEs, so a permissive parent cannot widen
/// access. The handle must have been opened with `WRITE_DAC` (via [`open_dir_no_follow`] `writable`).
pub fn apply_restrictive_dacl(handle: &OwnedHandle) -> io::Result<()> {
    let sids = principals()?;
    let mut acl = build_dacl(&sids)?;
    // SAFETY: `handle` is a valid object opened WRITE_DAC; `acl` is a well-formed DACL buffer that
    // outlives the call; owner/group/sacl are null (unchanged).
    let rc = unsafe {
        SetSecurityInfo(
            handle.as_raw_handle() as Handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl.as_mut_ptr() as *mut c_void,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Read back an open handle's DACL and confirm it is exactly the restrictive one this module writes:
/// protected from inheritance, and granting `FILE_ALL_ACCESS` to the current user, SYSTEM, and
/// Administrators — with **no other ACE**. Any deviation (an extra principal, a missing one, an
/// unexpected mask, a non-protected or absent DACL) is an error, so callers fail closed.
pub fn verify_restrictive_dacl(handle: &OwnedHandle) -> io::Result<()> {
    let expected = principals()?;

    let mut dacl: *mut c_void = std::ptr::null_mut();
    let mut sd: *mut c_void = std::ptr::null_mut();
    // SAFETY: querying only the DACL; owner/group/sacl outputs are null (not requested). `sd` receives
    // a LocalAlloc'd descriptor we free below; `dacl` points inside it.
    let rc = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle() as Handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    // Free the descriptor no matter which branch we leave by.
    let result = verify_dacl_contents(sd, dacl, &expected);
    if !sd.is_null() {
        // SAFETY: `sd` was allocated by GetSecurityInfo and is freed exactly once here.
        unsafe {
            LocalFree(sd);
        }
    }
    result
}

fn verify_dacl_contents(sd: *mut c_void, dacl: *mut c_void, expected: &[Sid]) -> io::Result<()> {
    // A NULL DACL grants everyone full access — the opposite of what we require.
    if dacl.is_null() {
        return Err(io::Error::other("object has a NULL DACL (grants everyone)"));
    }
    // The DACL must be protected, or inheritance could add ACEs we did not write.
    let mut control: u16 = 0;
    let mut revision: Dword = 0;
    // SAFETY: `sd` is a valid security descriptor from GetSecurityInfo.
    let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(io::Error::other("DACL is not protected from inheritance"));
    }

    let mut size_info = AclSizeInformation {
        ace_count: 0,
        acl_bytes_in_use: 0,
        acl_bytes_free: 0,
    };
    // SAFETY: `dacl` is a valid ACL; `size_info` is the matching output struct and size.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            &mut size_info as *mut _ as *mut c_void,
            std::mem::size_of::<AclSizeInformation>() as Dword,
            ACL_SIZE_INFORMATION,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    // Track which expected principals were seen; reject any ACE that is not one of them.
    let mut seen = vec![false; expected.len()];
    for i in 0..size_info.ace_count {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: `dacl` is valid and `i < ace_count`; `ace` receives a pointer into the ACL.
        let ok = unsafe { GetAce(dacl, i, &mut ace) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `ace` points at a valid ACE within the ACL for the duration of this read.
        let header = unsafe { &*(ace as *const AceHeader) };
        if header.ace_type != ACCESS_ALLOWED_ACE_TYPE {
            return Err(io::Error::other(
                "DACL contains a non-allow ACE; not the expected restrictive DACL",
            ));
        }
        // SAFETY: an ACCESS_ALLOWED_ACE begins at `ace`; its SID starts at the `sid_start` field.
        let allow = unsafe { &*(ace as *const AccessAllowedAce) };
        if allow.mask != FILE_ALL_ACCESS {
            return Err(io::Error::other(
                "DACL ACE grants a mask other than full control",
            ));
        }
        let ace_sid = std::ptr::addr_of!(allow.sid_start) as *mut c_void;
        let mut matched = false;
        for (idx, sid) in expected.iter().enumerate() {
            // SAFETY: both are valid SIDs (ours by construction, the ACE's by being in a valid ACL).
            if unsafe { EqualSid(ace_sid, sid.as_ptr()) } != 0 {
                seen[idx] = true;
                matched = true;
                break;
            }
        }
        if !matched {
            return Err(io::Error::other(
                "DACL grants a principal other than the current user, SYSTEM, or Administrators",
            ));
        }
    }
    if !seen.iter().all(|&s| s) {
        return Err(io::Error::other(
            "DACL is missing one of the required principals",
        ));
    }
    Ok(())
}

/// Create a directory (if absent) and lock it to the current user, returning a held no-follow handle.
///
/// The handle is opened `WRITE_DAC` and kept open across the caller's use, so the verified directory
/// cannot be swapped for a reparse point or deleted while held. The DACL is applied and then
/// re-read on the **same handle**, so the object verified is provably the object secured — no
/// path-reopen TOCTOU ([f15]). Idempotent: an existing correctly-secured directory re-verifies.
pub fn create_secured_dir(path: &Path) -> io::Result<OwnedHandle> {
    // Create the directory if it is not there. `create_dir` races benignly with a concurrent
    // creator; either way we then open and secure the result by handle.
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let handle = open_dir_no_follow(path, true)?;
    apply_restrictive_dacl(&handle)?;
    verify_restrictive_dacl(&handle)?;
    Ok(handle)
}

/// Atomically write `contents` to `path` as a temp file secured to the current user, then rename it
/// into place. The temp file is created `CREATE_NEW` (so it cannot be an attacker's pre-existing
/// reparse point), locked with the restrictive DACL, verified, written, and renamed over `path`.
///
/// The ACL travels with the file across a same-volume rename, so the destination inherits the
/// restrictive DACL. Callers must ensure `path`'s parent is itself a secured directory.
pub fn write_secured_file(path: &Path, tmp: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    // Remove any stale temp from a crashed writer; CREATE_NEW would otherwise fail.
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let handle = open_file(
        tmp,
        GENERIC_READ | GENERIC_WRITE | WRITE_DAC | READ_CONTROL,
        CREATE_NEW,
    )?;
    apply_restrictive_dacl(&handle)?;
    verify_restrictive_dacl(&handle)?;
    {
        // Write through a File cloned from the handle so the bytes land in the secured object.
        let mut file = std::fs::File::from(handle.try_clone()?);
        file.write_all(contents)?;
        file.flush()?;
    }
    // Drop the write handle before renaming; the destination keeps the file's own ACL.
    drop(handle);
    // MoveFileExW(MOVEFILE_REPLACE_EXISTING) via std::fs::rename; retry a transient sharing loss.
    let mut last = None;
    for attempt in 0..10 {
        match std::fs::rename(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
            }
        }
    }
    std::fs::remove_file(tmp).ok();
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

/// Open an existing file no-follow (rejecting a reparse point), verify its DACL is the restrictive
/// one, and return its contents. Used to read the allowlist store: a store whose ACL was widened or
/// whose path was replaced by a reparse point is untrusted and fails closed here.
pub fn read_secured_file(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let handle = open_file(path, GENERIC_READ | READ_CONTROL, OPEN_EXISTING)?;
    verify_restrictive_dacl(&handle)?;
    let mut file = std::fs::File::from(handle);
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn temp_dir() -> TempDir {
        crate::testutil::temp_dir("cross-review-winsec-tests")
    }

    #[test]
    fn secured_dir_applies_and_verifies_a_protected_dacl() {
        let dir = temp_dir();
        let target = dir.join("secured");
        let handle = create_secured_dir(&target).expect("create secured dir");
        // The freshly applied DACL verifies on the same handle.
        verify_restrictive_dacl(&handle).expect("verify");
        // Re-opening independently and verifying also passes (the DACL persisted to disk).
        let reopened = open_dir_no_follow(&target, false).expect("reopen");
        verify_restrictive_dacl(&reopened).expect("verify reopened");
    }

    #[test]
    fn create_secured_dir_is_idempotent() {
        let dir = temp_dir();
        let target = dir.join("secured");
        let _first = create_secured_dir(&target).expect("first");
        // Second call on an already-secured dir re-verifies rather than failing.
        let _second = create_secured_dir(&target).expect("second");
    }

    #[test]
    fn a_default_dir_fails_verification() {
        // A plain directory created without our DACL inherits its parent's permissive ACL and is
        // not protected, so verification must reject it — proving the check is not a no-op.
        let dir = temp_dir();
        let plain = dir.join("plain");
        std::fs::create_dir(&plain).expect("mkdir");
        let handle = open_dir_no_follow(&plain, false).expect("open");
        assert!(
            verify_restrictive_dacl(&handle).is_err(),
            "an un-secured directory must fail the restrictive-DACL check"
        );
    }

    #[test]
    fn secured_file_round_trips_and_verifies() {
        let dir = temp_dir();
        // Parent is a secured dir, as the store contract requires.
        let _parent = create_secured_dir(dir.as_path()).expect("secure parent");
        let target = dir.join("store.json");
        let tmp = dir.join("store.json.tmp");
        write_secured_file(&target, &tmp, b"hello").expect("write");
        let back = read_secured_file(&target).expect("read");
        assert_eq!(back, b"hello");
        // The tmp is consumed by the rename.
        assert!(!tmp.exists());
    }

    #[test]
    fn reading_a_reparse_point_is_refused() {
        // A file replaced by (or created as) a directory reparse target would be a swap attack; we
        // cannot easily create a symlink without privilege in a unit test, so at least assert the
        // no-follow open of a missing path errors rather than silently succeeding.
        let dir = temp_dir();
        let missing = dir.join("nope.json");
        assert!(read_secured_file(&missing).is_err());
    }
}
