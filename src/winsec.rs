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

use std::ffi::{c_void, OsStr};
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
// Directory rights needed for a handle used as an `NtCreateFile` RootDirectory: list names and
// traverse into them.
const FILE_LIST_DIRECTORY: Dword = 0x0000_0001;
const FILE_TRAVERSE: Dword = 0x0000_0020;
// Needed by GetFileInformationByHandle (the reparse-point check in `finish_open`). CreateFileW grants
// it implicitly, but NtCreateFile grants exactly what is asked, so `nt_open` must request it.
const FILE_READ_ATTRIBUTES: Dword = 0x0000_0080;
// Share reads with other openers. Whether DELETE is shared is per-open: a *held* directory or a
// write temp omits it (so the object cannot be renamed/deleted out from under the handle, [f20]),
// but a brief *read* of the store file shares DELETE so it does not block a concurrent atomic
// replacement — the reader keeps its already-opened snapshot while the writer's rename swaps the name.
const FILE_SHARE_READ: Dword = 0x0000_0001;
const FILE_SHARE_WRITE: Dword = 0x0000_0002;
const FILE_SHARE_DELETE: Dword = 0x0000_0004;
const OPEN_EXISTING: Dword = 3;
// MoveFileExW flags. We deliberately do NOT set MOVEFILE_REPLACE_EXISTING (0x1): a rename onto an
// existing target must **fail**, which is the race-safe collision check for `staging → home` and the
// safety rail for every recovery move (f-c1/f-c4). MOVEFILE_WRITE_THROUGH flushes the rename to disk
// before returning, making the directory-entry change durable across power loss (f-b5).
const MOVEFILE_WRITE_THROUGH: Dword = 0x0000_0008;
const MOVEFILE_REPLACE_EXISTING: Dword = 0x0000_0001;
const FILE_FLAG_BACKUP_SEMANTICS: Dword = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: Dword = 0x0020_0000;
const FILE_ATTRIBUTE_NORMAL: Dword = 0x0000_0080;
const FILE_ATTRIBUTE_DIRECTORY: Dword = 0x0000_0010;
const FILE_ATTRIBUTE_REPARSE_POINT: Dword = 0x0000_0400;
// NTFS caps a path component at 255 UTF-16 units; refuse anything longer before it is cast into a
// `USHORT`-length UNICODE_STRING (where an oversized name would silently truncate).
const MAX_COMPONENT_UTF16: usize = 255;

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
    fn MoveFileExW(existing_name: *const u16, new_name: *const u16, flags: Dword) -> Bool;
    fn FlushFileBuffers(file: Handle) -> Bool;
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

// --- ntdll (handle-relative open) -----------------------------------------------------------------
//
// A file opened by full path re-resolves every ancestor component, so `CreateFileW`'s
// `FILE_FLAG_OPEN_REPARSE_POINT` guards only the *final* component: a junction swapped in for the
// parent directory would still be followed. To prove a store/credential file is genuinely a direct
// child of the directory whose DACL we verified, we open it **relative to that directory's handle**
// with `NtCreateFile` (`RootDirectory` = the held handle), a single leaf component with no
// separators, so the resolution can only reach a direct child of the verified object ([f15]/[f20]).
//
// Containment against a swapped-in junction/symlink is enforced by opening the leaf as a reparse
// point (`FILE_OPEN_REPARSE_POINT`) rather than following it, then rejecting any handle that carries
// `FILE_ATTRIBUTE_REPARSE_POINT` (`finish_open`). We deliberately do **not** use the `OBJ_DONT_REPARSE`
// object attribute: it fails the whole open with `STATUS_REPARSE_POINT_ENCOUNTERED` (0xC000050B)
// whenever *any* reparse is met during resolution, including the benign, transparent reparses a
// filesystem overlay / HSM minifilter (e.g. Windows `UnionFS`, cloud-files) posts on an ordinary
// create or open. That broke first-time profile setup on machines whose `%LOCALAPPDATA%` sits under
// such an overlay (issue #117). `FILE_OPEN_REPARSE_POINT` only affects the final component — which is
// the only component here — so a real name-surrogate junction at the leaf still surfaces as a
// reparse-point handle and is refused, while an overlay's transparent redirection is followed as the
// filter intends. A `FILE_CREATE` onto a pre-planted junction still fails closed with a name collision.

// OBJECT_ATTRIBUTES.Attributes
const OBJ_CASE_INSENSITIVE: Dword = 0x0000_0040;
// NtCreateFile CreateDisposition
const FILE_OPEN: Dword = 1;
const FILE_CREATE: Dword = 2;
const FILE_OPEN_IF: Dword = 3;
// NtCreateFile CreateOptions
const FILE_DIRECTORY_FILE: Dword = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: Dword = 0x0000_0040;
const FILE_SYNCHRONOUS_IO_NONALERT: Dword = 0x0000_0020;
// Open the final component as a reparse point instead of following it, so a name-surrogate junction
// at the leaf is returned as a reparse-point handle (rejected by `finish_open`) rather than followed.
// Unlike the `OBJ_DONT_REPARSE` object attribute, it does not fail on an overlay filter's transparent
// reparse (issue #117). This is the CreateOptions counterpart of Win32 `FILE_FLAG_OPEN_REPARSE_POINT`.
const FILE_OPEN_REPARSE_POINT: Dword = 0x0020_0000;
// ACCESS_MASK: required alongside FILE_SYNCHRONOUS_IO_NONALERT.
const SYNCHRONIZE: Dword = 0x0010_0000;
const STATUS_SUCCESS: i32 = 0;
// NtCreateFile with FILE_CREATE returns this when the target already exists. Mapped to
// `io::ErrorKind::AlreadyExists` so an exclusive create can be distinguished from any other failure
// (the ownership proof for first-provision, impl-plan f1).
const STATUS_OBJECT_NAME_COLLISION: i32 = -1073741771; // 0xC0000035
const ERROR_ALREADY_EXISTS: i32 = 183;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: Dword,
    root_directory: Handle,
    object_name: *const UnicodeString,
    attributes: Dword,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}

#[repr(C)]
struct IoStatusBlock {
    // A union of NTSTATUS and PVOID; pointer-sized. We never read it, but its layout must be right.
    status_or_pointer: usize,
    information: usize,
}

#[link(name = "ntdll")]
extern "system" {
    #[allow(clippy::too_many_arguments)]
    fn NtCreateFile(
        file_handle: *mut Handle,
        desired_access: Dword,
        object_attributes: *const ObjectAttributes,
        io_status_block: *mut IoStatusBlock,
        allocation_size: *mut i64,
        file_attributes: Dword,
        share_access: Dword,
        create_disposition: Dword,
        create_options: Dword,
        ea_buffer: *mut c_void,
        ea_length: Dword,
    ) -> i32;
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

/// The principals every cross-review credential object grants, and nobody else: the current user,
/// plus SYSTEM and Administrators per Windows norms (a machine admin can already read anything;
/// denying them buys nothing and breaks backup/AV).
///
/// Deduplicated by SID: when the process itself runs as LocalSystem the current-user SID *is* the
/// SYSTEM SID, and a naive three-entry set would emit a duplicate ACE that `apply` writes but
/// `verify` then rejects (it marks the first matching slot and reports the twin missing). The set is
/// therefore the *distinct* principals, so apply and verify agree in every token context.
fn principals() -> io::Result<Vec<Sid>> {
    let mut sids: Vec<Sid> = Vec::with_capacity(3);
    for sid in [
        current_user_sid()?,
        well_known_sid(WIN_LOCAL_SYSTEM_SID)?,
        well_known_sid(WIN_BUILTIN_ADMINISTRATORS_SID)?,
    ] {
        // SAFETY: both operands are valid SIDs; EqualSid compares them by value.
        let dup = sids
            .iter()
            .any(|existing| unsafe { EqualSid(existing.as_ptr(), sid.as_ptr()) } != 0);
        if !dup {
            sids.push(sid);
        }
    }
    Ok(sids)
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

/// Split a path into its parent directory and its final component, rejecting a path with no parent
/// or no file name (both required for a handle-relative open of the child within its directory).
fn split_parent_leaf(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("path has no parent directory"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no final component"))?;
    Ok((parent, leaf))
}

/// Open a direct **file** child of `parent` by handle-relative resolution.
fn open_child_relative(
    parent: &OwnedHandle,
    leaf: &OsStr,
    access: Dword,
    disposition: Dword,
    share_delete: bool,
) -> io::Result<OwnedHandle> {
    nt_open(
        parent,
        leaf,
        access,
        disposition,
        FILE_NON_DIRECTORY_FILE,
        share_delete,
    )
}

/// Open (creating if absent) a direct **subdirectory** of `parent` by handle-relative resolution and
/// lock it to the current user with the restrictive DACL, returning the held no-follow handle.
///
/// Because the open is handle-relative (leaf opened as a reparse point, not followed), the
/// subdirectory is provably a direct child of `parent` — a junction swapped in at this level is
/// refused rather than redirecting — and no ancestor path component is re-resolved. The handle
/// carries `WRITE_DAC` (to set the DACL),
/// `READ_CONTROL` (to verify it), and list/traverse (so it can in turn be a RootDirectory for the next
/// level down). Idempotent: an existing correctly-secured child re-verifies.
pub fn create_secured_child_dir(parent: &OwnedHandle, leaf: &OsStr) -> io::Result<OwnedHandle> {
    let handle = nt_open(
        parent,
        leaf,
        READ_CONTROL | WRITE_DAC | FILE_LIST_DIRECTORY | FILE_TRAVERSE,
        FILE_OPEN_IF,
        FILE_DIRECTORY_FILE,
        false,
    )?;
    apply_restrictive_dacl(&handle)?;
    verify_restrictive_dacl(&handle)?;
    Ok(handle)
}

/// Like [`create_secured_child_dir`] but **exclusive** (`FILE_CREATE`): fails with
/// [`io::ErrorKind::AlreadyExists`] if `leaf` already exists rather than opening it. The successful
/// exclusive create is the **ownership proof** the setup flow needs — a nonce-named staging dir that
/// this call created is provably this run's, so rollback can delete it without ever adopting and then
/// destroying a directory it did not create (impl-plan f1/f-a1). Locked and verified through the same
/// handle it created.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn create_new_secured_child_dir(parent: &OwnedHandle, leaf: &OsStr) -> io::Result<OwnedHandle> {
    let handle = nt_open(
        parent,
        leaf,
        READ_CONTROL | WRITE_DAC | FILE_LIST_DIRECTORY | FILE_TRAVERSE,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
        false,
    )?;
    apply_restrictive_dacl(&handle)?;
    verify_restrictive_dacl(&handle)?;
    Ok(handle)
}

/// Prove a credential **file** (`auth.json`, `.credentials.json`, `.claude.json`) is a direct child of
/// the held `parent` directory and lock it to the current user, then verify.
///
/// The vendor login writes its credential file **by path** into the directory we handed it as
/// `CODEX_HOME` / `CLAUDE_CONFIG_DIR`, so after it exits we re-establish two guarantees on that file
/// ([f20]): (1) **structural containment** — the open is handle-relative (`RootDirectory = parent`,
/// leaf opened as a reparse point and refused if it is one), so it can only resolve a direct child of
/// the object we hold, not a reparse or replacement; and (2) **lockdown** — because the directory's
/// restrictive ACEs are non-inheritable, a
/// freshly written child does *not* inherit them, so a verify-only re-read would fail closed on a
/// legitimate login. We therefore **apply** the restrictive DACL to the file and then **verify** it,
/// mirroring what [`create_secured_dir`] does for a directory. Fails closed on any error.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn secure_and_verify_child_file(parent: &OwnedHandle, leaf: &OsStr) -> io::Result<()> {
    let handle = open_child_relative(
        parent,
        leaf,
        GENERIC_READ | WRITE_DAC | READ_CONTROL,
        FILE_OPEN,
        false,
    )?;
    apply_restrictive_dacl(&handle)?;
    verify_restrictive_dacl(&handle)?;
    Ok(())
}

/// Read a credential/account file that is a **direct child of the held `parent` directory**,
/// resolved handle-relative (`RootDirectory = parent`, leaf opened as a reparse point, not followed).
///
/// The setup confirmation reads the account fingerprint through this, not by path, so a late reparse
/// or replacement between the [f20] verify and the read cannot make the authorized account describe a
/// different object than the home we hold (f-a5). Structural containment is the guarantee; the file's
/// own DACL was already applied+verified by [`secure_and_verify_child_file`].
#[allow(dead_code)] // production caller lands with the setup confirmation probe (#15 part 3b).
pub fn read_child_file(parent: &OwnedHandle, leaf: &OsStr) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let handle = open_child_relative(parent, leaf, GENERIC_READ | READ_CONTROL, FILE_OPEN, false)?;
    let mut f = std::fs::File::from(handle);
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Reject a path any of whose **existing original components** is a reparse point (junction/symlink),
/// checked *before* canonicalization.
///
/// A junction placed at a pre-canonical component resolves to an ordinary directory, so a
/// canonicalize-then-check test would see straight through it; this walks the original path and opens
/// each existing prefix no-follow, refusing if any is a reparse point. Used for the explicit-home
/// escape hatch, whose ancestors are arbitrary directories not under the profile root (a `Named`
/// profile instead descends handle-relative from the trusted base, where every level is checked
/// structurally). A non-existent prefix is fine — it will be created. There is a residual TOCTOU on an
/// ancestor between this check and use, accepted for the local/trusted-only explicit-home path.
pub fn reject_reparse_on_ancestors(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let mut prefix = std::path::PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        match std::fs::symlink_metadata(&prefix) {
            Ok(md) => {
                if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(io::Error::other(format!(
                        "path component {} is a reparse point (junction/symlink); refusing",
                        prefix.display()
                    )));
                }
            }
            // A component that does not exist yet cannot be a reparse point; it will be created.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The shared `NtCreateFile` handle-relative open, rejecting a reparse point and any name that is not
/// a single path component. `RootDirectory = parent` with a separator-free leaf can only reach a
/// direct child of the held directory object, so containment is structural — no ancestor path
/// component is re-resolved ([f15]/[f20]). The leaf is opened as a reparse point
/// (`FILE_OPEN_REPARSE_POINT`) rather than followed, and `finish_open` refuses any handle bearing
/// `FILE_ATTRIBUTE_REPARSE_POINT`, so a name-surrogate junction/symlink at the leaf is rejected while
/// an overlay filter's transparent reparse is tolerated (issue #117; see the module note above).
/// `create_options` selects file vs directory; `disposition` selects open/create/open-if.
fn nt_open(
    parent: &OwnedHandle,
    leaf: &OsStr,
    access: Dword,
    disposition: Dword,
    kind_option: Dword,
    share_delete: bool,
) -> io::Result<OwnedHandle> {
    let name: Vec<u16> = leaf.encode_wide().collect();
    let is_dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
    if name.is_empty()
        || is_dot
        || name
            .iter()
            .any(|&c| c == u16::from(b'\\') || c == u16::from(b'/'))
    {
        return Err(io::Error::other(
            "a handle-relative open requires a single path component (no separators)",
        ));
    }
    // Bound the length before the USHORT cast below, so an oversized name cannot be silently
    // truncated into a shorter one that opens a *different* object.
    if name.len() > MAX_COMPONENT_UTF16 {
        return Err(io::Error::other(
            "path component exceeds the maximum length for a handle-relative open",
        ));
    }
    let byte_len = (name.len() * 2) as u16;
    let unicode = UnicodeString {
        length: byte_len,
        maximum_length: byte_len,
        buffer: name.as_ptr() as *mut u16,
    };
    let oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as Dword,
        root_directory: parent.as_raw_handle() as Handle,
        object_name: &unicode,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    if share_delete {
        share |= FILE_SHARE_DELETE;
    }
    let mut handle: Handle = std::ptr::null_mut();
    let mut iosb = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    // SAFETY: every pointer references a local (`unicode`, `name`, `oa`, `iosb`, `handle`) that lives
    // across the call; `name` backs the UNICODE_STRING buffer; the parent handle is valid and held.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            &oa,
            &mut iosb,
            std::ptr::null_mut(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            kind_option | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null_mut(),
            0,
        )
    };
    if status == STATUS_OBJECT_NAME_COLLISION {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{leaf:?} already exists"),
        ));
    }
    if status != STATUS_SUCCESS {
        return Err(io::Error::other(format!(
            "handle-relative open of {leaf:?} failed (NTSTATUS {status:#010x})"
        )));
    }
    finish_open(handle)
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
        // Required to obtain a handle to a *directory* rather than a file, and the list/traverse
        // rights let this handle serve as an `NtCreateFile` RootDirectory for handle-relative opens.
        flags |= FILE_FLAG_BACKUP_SEMANTICS;
        access |= FILE_LIST_DIRECTORY | FILE_TRAVERSE;
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
    let handle = finish_open(raw)?;
    // Require the opened object to actually be a directory when one was asked for. `create_dir`
    // reports an existing *file* as `AlreadyExists`, and a backup-semantics open will happily open a
    // file, so without this an explicit-home path naming a file (or, via a trailing `..`, a parent
    // directory or drive root) could receive the restrictive DACL — the wrong object entirely (f1).
    if is_dir {
        let attrs = handle_attributes(&handle)?;
        if attrs & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::other(
                "path is not a directory; refusing to treat it as a profile/credential directory",
            ));
        }
    }
    Ok(handle)
}

fn finish_open(raw: Handle) -> io::Result<OwnedHandle> {
    if raw as isize == INVALID_HANDLE_VALUE || raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid, owned handle from either `CreateFileW` (open_no_follow) or `NtCreateFile`
    // (nt_open); OwnedHandle closes it exactly once on drop.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as *mut _) };
    if handle_attributes(&handle)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(
            "path is a reparse point (junction/symlink); refusing to treat it as a real object",
        ));
    }
    Ok(handle)
}

/// The Win32 file attributes of an open handle.
fn handle_attributes(handle: &OwnedHandle) -> io::Result<Dword> {
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is valid; `info` is a correctly sized output struct.
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle() as Handle, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.file_attributes)
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
/// into place. The parent directory is opened no-follow, its restrictive DACL verified, and **held**
/// (no delete-sharing) across the whole operation, so it cannot be swapped or removed mid-write. The
/// temp file is created **relative to that held directory handle** (`FILE_CREATE`, so it cannot be an
/// attacker's pre-existing reparse point), locked with the restrictive DACL, verified, written, and
/// renamed over `path`.
///
/// The ACL travels with the file across the same-directory rename, so the destination keeps the
/// restrictive DACL. `path` and `tmp` must be siblings in a directory that is already a secured
/// directory ([`create_secured_dir`]); `tmp` is the sibling temp name.
///
/// `cancel`, when set, is checked immediately before each rename attempt — the atomic commit boundary
/// — so a caller can abort *before* the temp file is published, discarding it, without any of the
/// preceding I/O having taken effect. Cancellation returns [`io::ErrorKind::Interrupted`].
pub fn write_secured_file(
    path: &Path,
    tmp: &Path,
    contents: &[u8],
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::Ordering;

    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::SeqCst));

    let (dir, _leaf) = split_parent_leaf(path)?;
    let tmp_leaf = tmp
        .file_name()
        .ok_or_else(|| io::Error::other("temp path has no final component"))?;

    // Verify and hold the parent directory across create+write+rename. `open_dir_no_follow` shares
    // reads/writes but not DELETE, so while this handle lives the directory cannot be renamed or
    // deleted, and the rename below therefore resolves within the object whose DACL we verified.
    let dir_handle = open_dir_no_follow(dir, false)?;
    verify_restrictive_dacl(&dir_handle)?;

    // Remove any stale temp from a crashed writer; FILE_CREATE would otherwise fail.
    match std::fs::remove_file(tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let handle = open_child_relative(
        &dir_handle,
        tmp_leaf,
        GENERIC_READ | GENERIC_WRITE | WRITE_DAC | READ_CONTROL,
        FILE_CREATE,
        // The temp is ours alone until we rename it; do not share DELETE.
        false,
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
        // Cancellation is honoured at the atomic commit boundary: if it wins the race with the rename,
        // nothing is published — discard the temp and report interrupted. Checked without holding any
        // caller lock across the I/O below.
        if cancelled() {
            std::fs::remove_file(tmp).ok();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "write cancelled",
            ));
        }
        match std::fs::rename(tmp, path) {
            Ok(()) => {
                drop(dir_handle);
                return Ok(());
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
            }
        }
    }
    std::fs::remove_file(tmp).ok();
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

/// Read a secured file's contents, proving it is a direct child of its **verified** parent directory.
///
/// The parent is opened no-follow and its restrictive DACL verified; the file is then opened
/// *relative to that handle* (rejecting a reparse point) and its own DACL verified before the read.
/// This closes the gap a by-path open leaves — `FILE_FLAG_OPEN_REPARSE_POINT` guards only the final
/// component, so a junction swapped in for the parent could otherwise redirect to a file whose own
/// DACL happens to verify ([f22]). Any widened ACL, reparse point, or unverifiable parent fails
/// closed here, so the allowlist store refuses rather than trusting a tampered file.
pub fn read_secured_file(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let (dir, leaf) = split_parent_leaf(path)?;
    let dir_handle = open_dir_no_follow(dir, false)?;
    verify_restrictive_dacl(&dir_handle)?;
    // Share DELETE on the read so a held read handle never blocks a concurrent atomic replacement:
    // `is_authorized` reads without the store lock, so a writer's rename-over must not be able to
    // fail merely because a reader has the old file open. The reader keeps its opened snapshot.
    let file = open_child_relative(
        &dir_handle,
        leaf,
        GENERIC_READ | READ_CONTROL,
        FILE_OPEN,
        true,
    )?;
    verify_restrictive_dacl(&file)?;
    let mut f = std::fs::File::from(file);
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    // The parent handle is held until here so the child open above resolved within the verified dir.
    drop(dir_handle);
    Ok(buf)
}

/// Rename `from` to `to` **without replacing an existing target**, flushing the move to disk before
/// returning.
///
/// `std::fs::rename` maps to `MoveFileExW` **with** `MOVEFILE_REPLACE_EXISTING`, which on NTFS can
/// silently replace an existing *empty* directory — so it cannot be used where the rename doubles as a
/// collision check. This uses `MoveFileExW` with **no** replace flag, so a `to` that already exists
/// makes the call fail with [`io::ErrorKind::AlreadyExists`]; that is the race-safe guarantee behind
/// `staging → home` and every recovery move (f-c1/f-c4). `MOVEFILE_WRITE_THROUGH` makes the
/// directory-entry change durable across power loss (f-b5).
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from_w = wide(from);
    let to_w = wide(to);
    // SAFETY: both buffers are valid NUL-terminated UTF-16 paths that outlive the call.
    let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        // Normalise the "target exists" case to AlreadyExists so callers can branch on it uniformly.
        if err.raw_os_error() == Some(ERROR_ALREADY_EXISTS) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("cannot rename onto existing {}", to.display()),
            ));
        }
        return Err(err);
    }
    Ok(())
}

/// Rename `from` **over** `to` (replacing an existing target), flushing the move to disk before
/// returning (`MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`). Used for the write-ahead journal,
/// whose atomic replace must itself be durable across power loss, not just fail-open (f1/f-b3). Unlike
/// [`rename_no_replace`] this is *for* replacing (the journal file is overwritten each update).
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn rename_replace_write_through(from: &Path, to: &Path) -> io::Result<()> {
    let from_w = wide(from);
    let to_w = wide(to);
    // SAFETY: both buffers are valid NUL-terminated UTF-16 paths that outlive the call.
    let ok = unsafe {
        MoveFileExW(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Flush a file's contents and metadata to stable storage (`FlushFileBuffers`), so a
/// just-written/verified file survives a power loss (f-b2). The handle is opened no-follow with
/// `GENERIC_WRITE` (required by `FlushFileBuffers`; the current user's DACL grants it) and does not
/// modify the contents.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn flush_file(path: &Path) -> io::Result<()> {
    let wide = wide(path);
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path; other args are scalars.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    let handle = finish_open(raw)?;
    flush_handle(&handle)
}

/// Best-effort flush of a directory's metadata to stable storage after a rename/unlink changed its
/// entries. The durable-rename guarantee already comes from `MOVEFILE_WRITE_THROUGH` in
/// [`rename_no_replace`]; this is a belt-and-suspenders parent-directory barrier (f-b3/f-b5). Because
/// Windows does not universally support `FlushFileBuffers` on a directory handle, an
/// `ACCESS_DENIED`/`INVALID_PARAMETER` "not supported here" result is tolerated rather than treated as
/// a durability failure — the write-through rename is the load-bearing guarantee.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn flush_dir(path: &Path) -> io::Result<()> {
    let handle = match open_no_follow(path, true, true, OPEN_EXISTING) {
        Ok(h) => h,
        Err(_) => return Ok(()), // best-effort: a dir we cannot open write is not a hard failure here
    };
    match flush_handle(&handle) {
        Ok(()) => Ok(()),
        // Directory flush is not supported on every filesystem/handle; the write-through rename holds.
        Err(e) if matches!(e.raw_os_error(), Some(5) | Some(87)) => Ok(()),
        Err(e) => Err(e),
    }
}

fn flush_handle(handle: &OwnedHandle) -> io::Result<()> {
    // SAFETY: `handle` is a valid, owned handle opened with write access.
    let ok = unsafe { FlushFileBuffers(handle.as_raw_handle() as Handle) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The stable object identity of the directory at `path` — its NTFS `(volume serial, file index)`,
/// which a rename preserves. Opened **no-follow** (reparse points refused). Captured before a
/// `home → .old` move and re-checked at recovery, so a post-crash reparse or replacement at that path
/// is detected before recovery restores or removes it (f-a3). Returns `(volume_serial, file_index)`.
#[allow(dead_code)] // production caller lands with the setup provisioning flow (#15 part 3b).
pub fn dir_identity_no_follow(path: &Path) -> io::Result<(u32, u64)> {
    let handle = open_dir_no_follow(path, false)?;
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is valid; `info` is a correctly sized output struct.
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle() as Handle, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low);
    Ok((info.volume_serial_number, index))
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
        write_secured_file(&target, &tmp, b"hello", None).expect("write");
        let back = read_secured_file(&target).expect("read");
        assert_eq!(back, b"hello");
        // The tmp is consumed by the rename.
        assert!(!tmp.exists());
    }

    #[test]
    fn a_cancelled_secured_write_publishes_nothing() {
        // The cancel flag is honoured at the atomic commit boundary: a pre-cancelled write does the
        // preceding I/O but never renames the temp into place, so `path` is not created and the temp
        // is discarded (f1).
        use std::sync::atomic::AtomicBool;
        let dir = temp_dir();
        let _parent = create_secured_dir(dir.as_path()).expect("secure parent");
        let target = dir.join("store.json");
        let tmp = dir.join("store.json.tmp");
        let cancelled = AtomicBool::new(true);
        let err = write_secured_file(&target, &tmp, b"nope", Some(&cancelled))
            .expect_err("a cancelled write must not publish");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(!target.exists(), "nothing was published");
        assert!(!tmp.exists(), "the temp was discarded");
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

    #[test]
    fn a_shared_delete_reader_does_not_block_a_secured_rewrite() {
        // The store read path opens with FILE_SHARE_DELETE so a held read handle never blocks a
        // concurrent atomic replacement. Prove the mechanism: hold the target open with that exact
        // share mode, then a secured rewrite (rename-over) must still succeed and be visible.
        use std::os::windows::fs::OpenOptionsExt;
        let dir = temp_dir();
        let _parent = create_secured_dir(dir.as_path()).expect("secure parent");
        let target = dir.join("store.json");
        let tmp = dir.join("store.json.tmp");
        write_secured_file(&target, &tmp, b"v1", None).expect("write v1");
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&target)
            .expect("hold a shared-delete reader");
        let tmp2 = dir.join("store.json.tmp2");
        write_secured_file(&target, &tmp2, b"v2", None)
            .expect("a rewrite must not be blocked by a shared-delete reader");
        assert_eq!(read_secured_file(&target).expect("read"), b"v2");
    }

    #[test]
    fn reject_reparse_on_ancestors_refuses_a_junctioned_component() {
        let dir = temp_dir();
        let real = dir.join("real");
        std::fs::create_dir_all(&real).expect("mkdir real");
        let link = dir.join("link");
        assert!(
            crate::testutil::make_junction(&link, &real),
            "could not create a junction to exercise the check"
        );
        // A path whose ancestor is the junction is refused...
        assert!(reject_reparse_on_ancestors(&link.join("child")).is_err());
        // ...while the same path through the real directory is accepted, and a not-yet-created
        // component is fine (it will be made).
        assert!(reject_reparse_on_ancestors(&real.join("child")).is_ok());
    }

    #[test]
    fn a_file_in_an_unsecured_parent_is_refused_on_read() {
        // [f22]/f1: the read path must verify the PARENT directory, not only the file. A file whose
        // own contents look fine but which sits in an unsecured (default-ACL) directory must fail
        // closed — otherwise a widened or junctioned parent could redirect the read. Here the parent
        // is a plain directory (never create_secured_dir'd), so the parent-DACL check must reject it.
        let dir = temp_dir();
        let plain_parent = dir.join("plain");
        std::fs::create_dir_all(&plain_parent).expect("mkdir");
        let file = plain_parent.join("store.json");
        std::fs::write(&file, b"{}").expect("write plain file");
        assert!(
            read_secured_file(&file).is_err(),
            "a file under an unsecured parent directory must fail closed"
        );
    }

    #[test]
    fn create_new_secured_child_dir_is_exclusive() {
        // The exclusive create is the ownership proof: it succeeds once and then reports AlreadyExists,
        // never silently adopting a pre-existing directory (f1/f-a1).
        let dir = temp_dir();
        let parent = create_secured_dir(dir.as_path()).expect("secure parent");
        let first =
            create_new_secured_child_dir(&parent, OsStr::new("stage")).expect("first create");
        verify_restrictive_dacl(&first).expect("child is locked");
        let err = create_new_secured_child_dir(&parent, OsStr::new("stage"))
            .expect_err("a second exclusive create must be refused");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn a_junction_at_the_leaf_is_refused_by_handle_relative_open() {
        // Containment ([f15]/[f20]): the descent opens each leaf as a reparse point rather than
        // following it, so a name-surrogate junction swapped in at a child name is refused instead of
        // redirecting the open to its target. This is the security property the #117 fix preserves
        // after dropping OBJ_DONT_REPARSE (which would fail the whole open) in favour of
        // FILE_OPEN_REPARSE_POINT + the finish_open reparse-attribute rejection.
        let dir = temp_dir();
        let parent = create_secured_dir(dir.as_path()).expect("secure parent");
        let escape = dir.join("escape");
        std::fs::create_dir_all(&escape).expect("mkdir escape target");
        let link = dir.join("link");
        assert!(
            crate::testutil::make_junction(&link, &escape),
            "could not create a junction to exercise the check"
        );
        // The descent opens each level as a directory (FILE_OPEN_IF): meeting the junction here opens
        // the reparse point itself (FILE_OPEN_REPARSE_POINT) rather than following it, and finish_open
        // refuses the reparse-point handle instead of adopting the junction's target.
        let err = create_secured_child_dir(&parent, OsStr::new("link"))
            .expect_err("a junction at the leaf must be refused, not followed");
        assert!(
            err.to_string().contains("reparse point"),
            "expected a reparse-point rejection, got: {err}"
        );
    }

    #[test]
    fn secure_and_verify_child_file_locks_a_child_and_rejects_a_widened_one() {
        // A vendor-written credential file does not inherit the (non-inheritable) directory DACL, so
        // apply-then-verify is required and a file left with a widened ACL fails ([f20]).
        let dir = temp_dir();
        let parent = create_secured_dir(dir.as_path()).expect("secure parent");
        // A plain file written into the secured dir (as the vendor login would) starts un-locked.
        std::fs::write(dir.join("auth.json"), b"{}").expect("write cred");
        // apply-then-verify locks it and passes.
        secure_and_verify_child_file(&parent, OsStr::new("auth.json")).expect("secure child file");
        // Re-opening independently and verifying also passes (the DACL persisted).
        let reopened = open_dir_no_follow(dir.as_path(), false).expect("reopen dir");
        let child = open_child_relative(
            &reopened,
            OsStr::new("auth.json"),
            GENERIC_READ | READ_CONTROL,
            FILE_OPEN,
            false,
        )
        .expect("open child");
        verify_restrictive_dacl(&child).expect("child stayed locked");
    }

    #[test]
    fn rename_no_replace_refuses_an_existing_target() {
        let dir = temp_dir();
        let from = dir.join("staging");
        let to = dir.join("home");
        std::fs::create_dir(&from).expect("mkdir from");
        // No collision: the move succeeds.
        rename_no_replace(&from, &to).expect("first move");
        assert!(to.exists() && !from.exists());
        // A second staging that collides with the now-existing target is refused, not merged/replaced.
        let from2 = dir.join("staging2");
        std::fs::create_dir(&from2).expect("mkdir from2");
        let err = rename_no_replace(&from2, &to).expect_err("must refuse an existing target");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(from2.exists(), "the source is left in place on refusal");
    }

    #[test]
    fn dir_identity_is_stable_across_a_rename_and_distinct_between_dirs() {
        // A rename preserves the (volume, file-index) identity; two different dirs differ. This is what
        // lets recovery prove `.old` is the object it moved aside, not a replacement (f-a3).
        let dir = temp_dir();
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir(&a).expect("mkdir a");
        std::fs::create_dir(&b).expect("mkdir b");
        let id_a = dir_identity_no_follow(&a).expect("id a");
        let id_b = dir_identity_no_follow(&b).expect("id b");
        assert_ne!(id_a, id_b, "distinct directories have distinct identities");
        let moved = dir.join("a-moved");
        rename_no_replace(&a, &moved).expect("rename a");
        let id_moved = dir_identity_no_follow(&moved).expect("id moved");
        assert_eq!(id_a, id_moved, "a rename preserves the object identity");
    }

    #[test]
    fn flush_file_flushes_without_altering_contents() {
        let dir = temp_dir();
        let f = dir.join("cred.json");
        std::fs::write(&f, b"{\"a\":1}").expect("write");
        flush_file(&f).expect("flush");
        assert_eq!(std::fs::read(&f).expect("read"), b"{\"a\":1}");
        // flush_dir is best-effort and must not error on an ordinary directory.
        flush_dir(dir.as_path()).expect("flush dir best-effort");
    }
}
