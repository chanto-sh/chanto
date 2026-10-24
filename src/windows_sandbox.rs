//! Unprivileged Windows shell isolation: a write-restricted token plus a job.
//!
//! The command runs under a restricted copy of the user's own token, created
//! with WRITE_RESTRICTED. Windows checks a write-restricted token twice for
//! write access only: once with the user's normal SIDs and once with the
//! restricting SIDs, and both must grant it. Reads take the normal check
//! alone, so system files, .NET, the user profile and pre-existing project
//! files all stay readable (an AppContainer would need explicit read grants
//! for every one of them, which is why this design replaced it).
//!
//! Each writable root gets its own restricting SID, derived from its
//! canonical path, and an inheritable allow ACE for that SID. The ACE stays
//! on the root after the command (the first grant propagates it to existing
//! descendants, which can take a while on a large tree; later commands only
//! check it). Leaving it is harmless: a restricting SID only ever narrows a
//! token's access, so the ACE gives nothing to processes outside the sandbox.
//!
//! The other restricting SIDs are needed for the process to start: the logon
//! SID (window station, desktop), Everyone and RESTRICTED (system objects
//! such as the null device and named-object directories), and a per-launch
//! SID that the token's default DACL grants, so the child can open the
//! process, thread and other objects it creates. Locations that grant
//! Everyone or RESTRICTED write access stay writable; user files essentially
//! never do. The approach follows the unelevated backend studied in
//! codex-rs/windows-sandbox-rs; the implementation here is independent.
//!
//! Not enforced here, by design of WRITE_RESTRICTED: read denials (restricting
//! SIDs are not consulted for reads) and network access (only AppContainer or
//! administrator-installed firewall rules can block sockets).
//!
//! All native calls keep their backing buffers/handles alive through process
//! creation. Only the three duplicated standard handles are inheritable.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, c_void};
use std::marker::PhantomData;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{NonNull, null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::StationsAndDesktops::*;
use windows_sys::Win32::System::Threading::*;

pub const LAUNCH_FLAG: &str = "--internal-windows-sandbox";

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub writable: Vec<PathBuf>,
    pub parent_pid: u32,
}

fn wide(s: impl AsRef<OsStr>) -> Result<Vec<u16>> {
    let mut s: Vec<u16> = s.as_ref().encode_wide().collect();
    ensure!(!s.contains(&0), "NUL in Windows sandbox argument");
    s.push(0);
    Ok(s)
}

// Name the failing call: a bare OS error (e.g. "parameter is incorrect")
// does not say which of the many native steps rejected it.
fn check(ok: i32, call: &str) -> Result<()> {
    if ok == 0 {
        let e = std::io::Error::last_os_error();
        Err(anyhow::anyhow!("{call}: {e}"))
    } else {
        Ok(())
    }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

// LocalFree allocations (ACLs/security descriptors).
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

/// An owned SID buffer (u32 storage keeps the subauthorities aligned).
struct Sid(Vec<u32>);
impl Sid {
    // S-1-5-21-a-b-c-d: shaped like a domain account SID that matches no
    // real account.
    fn from_parts(parts: [u32; 4]) -> Self {
        let mut v = vec![0u32; 7];
        // Revision 1, five subauthorities, NT authority (5), big-endian.
        let header: [u8; 8] = [1, 5, 0, 0, 0, 0, 0, 5];
        v[0] = u32::from_ne_bytes(header[0..4].try_into().unwrap());
        v[1] = u32::from_ne_bytes(header[4..8].try_into().unwrap());
        v[2] = 21;
        v[3..7].copy_from_slice(&parts);
        Self(v)
    }
    fn random() -> Self {
        Self::from_bytes(uuid::Uuid::new_v4().into_bytes())
    }
    fn from_bytes(b: [u8; 16]) -> Self {
        let part = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        Self::from_parts([part(0), part(1), part(2), part(3)])
    }
    /// The stable restricting SID for one writable root. Anyone can compute
    /// it, which is fine: restricting SIDs can only reduce a token's access.
    fn for_root(path: &Path) -> Self {
        use sha2::Digest;
        let key = path.to_string_lossy().to_lowercase();
        let digest = sha2::Sha256::digest(format!("chanto-sandbox-write-root\0{key}").as_bytes());
        Self::from_bytes(digest[..16].try_into().unwrap())
    }
    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self> {
        let mut v = vec![0u32; 17]; // SECURITY_MAX_SID_SIZE (68 bytes)
        let mut bytes = (v.len() * 4) as u32;
        check(
            unsafe { CreateWellKnownSid(kind, null_mut(), v.as_mut_ptr().cast(), &mut bytes) },
            "CreateWellKnownSid",
        )?;
        Ok(Self(v))
    }
    fn copy_of(sid: PSID) -> Result<Self> {
        let len = unsafe { GetLengthSid(sid) };
        let mut v = vec![0u32; (len as usize).div_ceil(4)];
        check(
            unsafe { CopySid(len, v.as_mut_ptr().cast(), sid) },
            "CopySid",
        )?;
        Ok(Self(v))
    }
    fn ptr(&self) -> PSID {
        self.0.as_ptr() as PSID
    }
}

const INHERITABLE: u8 = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
// No WRITE_DAC/WRITE_OWNER: the child must not expand its own access.
const WRITE_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE | FILE_GENERIC_WRITE | DELETE;

// Serializes ACL edits across concurrent launchers. Windows mutexes are
// recursive, so nested acquisition by one thread is fine.
struct AclLock(Handle);
impl AclLock {
    fn new() -> Result<Self> {
        let name = wide("Local\\chanto-sandbox-acl")?;
        let mutex = Handle(unsafe { CreateMutexW(null(), 0, name.as_ptr()) });
        ensure!(
            !mutex.0.is_null(),
            "CreateMutexW: {}",
            std::io::Error::last_os_error()
        );
        // Long: a first grant propagates through a whole tree.
        let status = unsafe { WaitForSingleObject(mutex.0, 300_000) };
        ensure!(
            status == WAIT_OBJECT_0 || status == WAIT_ABANDONED,
            "WaitForSingleObject(sandbox ACL mutex): {status:#x}"
        );
        Ok(Self(mutex))
    }
}
impl Drop for AclLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0.0);
        }
    }
}

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;

/// A borrowed ACL: non-null, accepted by `IsValidAcl`, and stored in memory
/// its owner (a security descriptor, a token information buffer or a
/// [`Dacl`]'s storage) keeps in place for `'a`. Every raw ACL from Windows
/// goes through [`AclRef::new`] before anything reads it.
#[derive(Clone, Copy)]
struct AclRef<'a> {
    ptr: NonNull<ACL>,
    _owner: PhantomData<&'a ()>,
}

impl<'a> AclRef<'a> {
    /// `None` for a NULL ACL; an error if Windows says it is malformed.
    ///
    /// # Safety
    /// A non-null `ptr` must point at an ACL inside `_owner`, which must not
    /// move or free it while the result is in use.
    unsafe fn new<T: ?Sized>(ptr: *mut ACL, _owner: &'a T) -> Result<Option<Self>> {
        let Some(ptr) = NonNull::new(ptr) else {
            return Ok(None);
        };
        ensure!(
            unsafe { IsValidAcl(ptr.as_ptr()) } != 0,
            "IsValidAcl: malformed ACL"
        );
        Ok(Some(Self {
            ptr,
            _owner: PhantomData,
        }))
    }

    fn header(self) -> ACL {
        // SAFETY: a valid ACL starts with its header (the type's invariant).
        unsafe { self.ptr.as_ptr().read_unaligned() }
    }

    /// Its size in bytes, header and ACEs included.
    fn size(self) -> usize {
        self.header().AclSize as usize
    }

    fn ace_count(self) -> u32 {
        self.header().AceCount as u32
    }

    fn as_ptr(self) -> *mut ACL {
        self.ptr.as_ptr()
    }

    /// ACE `index`, checked to lie wholly within the ACL's own `AclSize`
    /// bytes.
    fn ace(self, index: u32) -> Result<Ace<'a>> {
        let mut ace: *mut c_void = null_mut();
        check(unsafe { GetAce(self.as_ptr(), index, &mut ace) }, "GetAce")?;
        let ace =
            NonNull::new(ace.cast::<u8>()).with_context(|| format!("GetAce: no ACE {index}"))?;
        let start = self.as_ptr() as usize + size_of::<ACL>();
        let end = self.as_ptr() as usize + self.size();
        let at = ace.as_ptr() as usize;
        ensure!(
            start <= at && at + size_of::<ACE_HEADER>() <= end,
            "ACE {index} lies outside its ACL"
        );
        // SAFETY: the header is within the ACL (checked just above).
        let header = unsafe { ace.cast::<ACE_HEADER>().as_ptr().read_unaligned() };
        let size = header.AceSize as usize;
        ensure!(
            size >= size_of::<ACE_HEADER>() && at + size <= end,
            "ACE {index} overruns its ACL"
        );
        Ok(Ace {
            ptr: ace,
            header,
            _acl: PhantomData,
        })
    }
}

/// One ACE of an [`AclRef`]: its `AceSize` bytes are all inside the ACL.
#[derive(Clone, Copy)]
struct Ace<'a> {
    ptr: NonNull<u8>,
    header: ACE_HEADER,
    _acl: PhantomData<&'a ()>,
}

impl Ace<'_> {
    fn header(self) -> ACE_HEADER {
        self.header
    }

    /// The access mask and SID of an access-allowed or access-denied ACE
    /// (the two share a layout), if it is one and it holds the whole SID.
    fn allowed_or_denied(self) -> Option<(u32, PSID)> {
        if !matches!(
            self.header.AceType,
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE
        ) {
            return None;
        }
        let mask_at = std::mem::offset_of!(ACCESS_ALLOWED_ACE, Mask);
        let sid_at = std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        // A SID is 8 bytes plus 4 per subauthority; byte 1 is their count.
        let size = self.header.AceSize as usize;
        if size < sid_at + 8 {
            return None;
        }
        // SAFETY: every offset read below is less than `size`, and the ACE's
        // `size` bytes are inside its ACL (the type's invariant).
        let subauthorities = unsafe { self.ptr.add(sid_at + 1).read() } as usize;
        if size < sid_at + 8 + 4 * subauthorities {
            return None;
        }
        let mask = unsafe { self.ptr.add(mask_at).cast::<u32>().read_unaligned() };
        let sid = unsafe { self.ptr.add(sid_at) }.as_ptr().cast();
        Some((mask, sid))
    }
}

// Whether a DACL already carries our explicit, inheritable write grant.
fn has_write_grant(acl: AclRef, sid: PSID, dir: bool) -> bool {
    (0..acl.ace_count()).any(|i| {
        let Ok(ace) = acl.ace(i) else {
            return false;
        };
        let header = ace.header();
        header.AceType == ACCESS_ALLOWED_ACE_TYPE
            && header.AceFlags & (INHERITED_ACE | INHERIT_ONLY_ACE) as u8 == 0
            && (!dir || header.AceFlags & INHERITABLE == INHERITABLE)
            && ace.allowed_or_denied().is_some_and(|(mask, ace_sid)| {
                mask & WRITE_MASK == WRITE_MASK && unsafe { EqualSid(ace_sid, sid) } != 0
            })
    })
}

/// A file's DACL, inside the security descriptor that holds it.
struct FileDacl {
    acl: *mut ACL,
    descriptor: Local,
}

impl FileDacl {
    /// `None` for a NULL DACL.
    fn acl(&self) -> Result<Option<AclRef<'_>>> {
        // SAFETY: `acl` is null or points into `descriptor`, freed only on drop.
        unsafe { AclRef::new(self.acl, &self.descriptor) }
    }
}

fn read_file_dacl(name: &[u16]) -> Result<FileDacl> {
    let mut acl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    let err = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    ensure!(
        err == 0,
        "GetNamedSecurityInfoW: {}",
        std::io::Error::from_raw_os_error(err as i32)
    );
    Ok(FileDacl {
        acl,
        descriptor: Local(descriptor),
    })
}

/// Give `sid` write access to `path` and (inheritably) everything under it.
/// Idempotent; the grant stays (see the module comment).
fn grant_write(path: &Path, sid: &Sid) -> Result<()> {
    let name = wide(path)?;
    let dir = path.is_dir();
    let _lock = AclLock::new()?;
    let dacl = read_file_dacl(&name)?;
    // A NULL DACL already grants everyone (and every restricting SID) access.
    let Some(acl) = dacl.acl()? else {
        return Ok(());
    };
    if has_write_grant(acl, sid.ptr(), dir) {
        return Ok(());
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: WRITE_MASK,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: if dir {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.ptr().cast(),
        },
    };
    let mut new_acl: *mut ACL = null_mut();
    let err = unsafe { SetEntriesInAclW(1, &entry, acl.as_ptr(), &mut new_acl) };
    ensure!(
        err == 0,
        "SetEntriesInAclW {}: {}",
        path.display(),
        std::io::Error::from_raw_os_error(err as i32)
    );
    let _new_acl = Local(new_acl.cast());
    // Unlike SetFileSecurityW this propagates the inheritable ACE to
    // existing descendants (except those with protected DACLs).
    let err = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_acl,
            null(),
        )
    };
    if err != 0 {
        // Propagation can fail on a descendant after the root was updated;
        // those descendants just stay read-only inside the sandbox.
        let dacl = read_file_dacl(&name)?;
        ensure!(
            dacl.acl()?
                .is_some_and(|acl| has_write_grant(acl, sid.ptr(), dir)),
            "SetNamedSecurityInfoW {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(err as i32)
        );
    }
    Ok(())
}

// A copy of a window station's or desktop's DACL with room for one more ACE.
// The storage is usize for alignment.
struct Dacl {
    handle: HANDLE,
    storage: Vec<usize>,
}
impl Dacl {
    fn read(handle: HANDLE, spare: usize) -> Result<Self> {
        let mut acl: *mut ACL = null_mut();
        let mut descriptor = null_mut();
        let err = unsafe {
            GetSecurityInfo(
                handle,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            )
        };
        ensure!(
            err == 0,
            "GetSecurityInfo(window station/desktop): {}",
            std::io::Error::from_raw_os_error(err as i32)
        );
        let descriptor = Local(descriptor);
        // SAFETY: GetSecurityInfo points `acl` into `descriptor`, or leaves it null.
        let Some(acl) = (unsafe { AclRef::new(acl, &descriptor) })? else {
            bail!("window station/desktop has no DACL");
        };
        let bytes = acl.size();
        let size = bytes + spare;
        ensure!(size <= u16::MAX as usize, "DACL too large");
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        unsafe {
            std::ptr::copy_nonoverlapping(
                acl.as_ptr().cast::<u8>(),
                storage.as_mut_ptr().cast(),
                bytes,
            );
            (*storage.as_mut_ptr().cast::<ACL>()).AclSize = size as u16;
        }
        Ok(Self { handle, storage })
    }
    fn acl(&mut self) -> *mut ACL {
        self.storage.as_mut_ptr().cast()
    }
    /// The copy, for reading: a valid ACL (a copy of one, with `AclSize`
    /// raised to cover the spare room) that `storage` holds.
    fn view(&self) -> AclRef<'_> {
        AclRef {
            ptr: NonNull::from(self.storage.as_slice()).cast(),
            _owner: PhantomData,
        }
    }
    fn write(&mut self) -> Result<()> {
        let mut sd: SECURITY_DESCRIPTOR = unsafe { zeroed() };
        let psd: PSECURITY_DESCRIPTOR = (&mut sd as *mut SECURITY_DESCRIPTOR).cast();
        check(
            unsafe {
                InitializeSecurityDescriptor(psd, 1 /* SECURITY_DESCRIPTOR_REVISION */)
            },
            "InitializeSecurityDescriptor",
        )?;
        check(
            unsafe { SetSecurityDescriptorDacl(psd, 1, self.acl(), 0) },
            "SetSecurityDescriptorDacl",
        )?;
        check(
            unsafe { SetUserObjectSecurity(self.handle, &DACL_SECURITY_INFORMATION, psd) },
            "SetUserObjectSecurity",
        )
    }
}

// user32 initialization in the child opens the inherited window station and
// desktop. The logon SID normally covers that; this per-launch grant covers
// sessions (e.g. services) whose window station names a different SID.
// Removed again when the launch ends.
struct DesktopGrant {
    winsta: HWINSTA,
    desktop: HDESK,
    sid: PSID,
}
impl DesktopGrant {
    fn new(sid: &Sid) -> Result<Self> {
        let _lock = AclLock::new()?;
        let name = |object: HANDLE| -> Result<Vec<u16>> {
            let mut name = vec![0u16; 256];
            let mut needed = 0;
            check(
                unsafe {
                    GetUserObjectInformationW(
                        object,
                        UOI_NAME,
                        name.as_mut_ptr().cast(),
                        (name.len() * 2) as u32,
                        &mut needed,
                    )
                },
                "GetUserObjectInformationW",
            )?;
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len() - 1);
            name.truncate(len);
            name.push(0);
            Ok(name)
        };
        let access = READ_CONTROL | WRITE_DAC;
        let winsta = name(unsafe { GetProcessWindowStation() })?;
        let desktop = name(unsafe { GetThreadDesktop(GetCurrentThreadId()) })?;
        let mut grant = Self {
            winsta: null_mut(),
            desktop: null_mut(),
            sid: sid.ptr(),
        };
        grant.winsta = unsafe { OpenWindowStationW(winsta.as_ptr(), 0, access) };
        ensure!(
            !grant.winsta.is_null(),
            "OpenWindowStationW: {}",
            std::io::Error::last_os_error()
        );
        grant.desktop = unsafe { OpenDesktopW(desktop.as_ptr(), 0, 0, access) };
        ensure!(
            !grant.desktop.is_null(),
            "OpenDesktopW: {}",
            std::io::Error::last_os_error()
        );
        // All window station rights; desktop rights without journaling or
        // switching desktops.
        add_window_ace(grant.winsta, grant.sid, 0x37f | READ_CONTROL)?;
        add_window_ace(grant.desktop, grant.sid, 0xcf | READ_CONTROL)?;
        Ok(grant)
    }
}
impl Drop for DesktopGrant {
    fn drop(&mut self) {
        if let Ok(_lock) = AclLock::new() {
            for object in [self.winsta, self.desktop] {
                if !object.is_null() {
                    let _ = remove_window_aces(object, self.sid);
                }
            }
        }
        unsafe {
            if !self.winsta.is_null() {
                CloseWindowStation(self.winsta);
            }
            if !self.desktop.is_null() {
                CloseDesktop(self.desktop);
            }
        }
    }
}

// Append one explicit allow ACE before any inherited entries.
fn add_window_ace(handle: HANDLE, sid: PSID, mask: u32) -> Result<()> {
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    let ace_len = size_of::<ACE_HEADER>() + size_of::<u32>() + sid_len;
    let mut dacl = Dacl::read(handle, ace_len)?;
    let mut ace = vec![0usize; ace_len.div_ceil(size_of::<usize>())];
    let new = ace.as_mut_ptr().cast::<ACCESS_ALLOWED_ACE>();
    unsafe {
        (*new).Header = ACE_HEADER {
            AceType: 0,
            AceFlags: 0,
            AceSize: ace_len as u16,
        };
        (*new).Mask = mask;
    }
    check(
        unsafe {
            CopySid(
                sid_len as u32,
                std::ptr::addr_of_mut!((*new).SidStart).cast(),
                sid,
            )
        },
        "CopySid",
    )?;
    let acl = dacl.view();
    let mut index = u32::MAX; // append
    for i in 0..acl.ace_count() {
        if acl.ace(i)?.header().AceFlags & INHERITED_ACE as u8 != 0 {
            index = i;
            break;
        }
    }
    let revision = acl.header().AclRevision as u32;
    check(
        unsafe {
            AddAce(
                dacl.acl(),
                revision,
                index,
                ace.as_ptr().cast(),
                ace_len as u32,
            )
        },
        "AddAce",
    )?;
    dacl.write()
}

fn remove_window_aces(handle: HANDLE, sid: PSID) -> Result<()> {
    let mut dacl = Dacl::read(handle, 0)?;
    let mut changed = false;
    // Backwards, so deleting an ACE leaves the indexes still to visit alone.
    for i in (0..dacl.view().ace_count()).rev() {
        let ours = dacl
            .view()
            .ace(i)?
            .allowed_or_denied()
            .is_some_and(|(_, ace_sid)| unsafe { EqualSid(ace_sid, sid) } != 0);
        if ours {
            check(unsafe { DeleteAce(dacl.acl(), i) }, "DeleteAce")?;
            changed = true;
        }
    }
    if changed {
        dacl.write()?;
    }
    Ok(())
}

fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<usize>> {
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token, class, null_mut(), 0, &mut bytes);
    }
    ensure!(bytes > 0, "GetTokenInformation({class}): no size");
    let mut info = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    check(
        unsafe { GetTokenInformation(token, class, info.as_mut_ptr().cast(), bytes, &mut bytes) },
        &format!("GetTokenInformation({class})"),
    )?;
    Ok(info)
}

const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;

// The logon-session SID, if the token has one.
fn logon_sid(token: HANDLE) -> Result<Option<Sid>> {
    let info = token_info(token, TokenGroups)?;
    let groups = info.as_ptr().cast::<TOKEN_GROUPS>();
    let count = unsafe { (*groups).GroupCount } as usize;
    let entries = unsafe { std::slice::from_raw_parts((*groups).Groups.as_ptr(), count) };
    entries
        .iter()
        .find(|g| g.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID)
        .map(|g| Sid::copy_of(g.Sid))
        .transpose()
}

// Objects the child creates (its process, threads, sections) take their DACL
// from the token's default DACL. Writes to them by a write-restricted token
// pass only if that DACL names both one of its normal SIDs and one of its
// restricting SIDs; otherwise the child cannot open its own objects and dies
// in initialization with STATUS_DLL_INIT_FAILED. An elevated token's default
// DACL names only Administrators and SYSTEM, so add the user too.
fn allow_in_default_dacl(token: HANDLE, sids: &[PSID]) -> Result<()> {
    let info = token_info(token, TokenDefaultDacl)?;
    let old = unsafe { (*info.as_ptr().cast::<TOKEN_DEFAULT_DACL>()).DefaultDacl };
    // SAFETY: the default DACL, if any, is stored in `info` after the pointer.
    // No default DACL: new objects get no DACL and are open to everyone.
    let Some(old) = (unsafe { AclRef::new(old, &info) })? else {
        return Ok(());
    };
    let old_size = old.size();
    let size = old_size
        + sids
            .iter()
            .map(|&sid| size_of::<ACCESS_ALLOWED_ACE>() + unsafe { GetLengthSid(sid) } as usize)
            .sum::<usize>();
    ensure!(size <= u16::MAX as usize, "default DACL too large");
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let acl = storage.as_mut_ptr().cast::<ACL>();
    unsafe {
        std::ptr::copy_nonoverlapping(old.as_ptr().cast::<u8>(), acl.cast::<u8>(), old_size);
        (*acl).AclSize = size as u16;
    }
    let revision = unsafe { (*acl).AclRevision as u32 };
    for &sid in sids {
        check(
            unsafe { AddAccessAllowedAce(acl, revision, GENERIC_ALL, sid) },
            "AddAccessAllowedAce",
        )?;
    }
    let dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    check(
        unsafe {
            SetTokenInformation(
                token,
                TokenDefaultDacl,
                (&dacl as *const TOKEN_DEFAULT_DACL).cast(),
                size_of::<TOKEN_DEFAULT_DACL>() as u32,
            )
        },
        "SetTokenInformation(TokenDefaultDacl)",
    )
}

struct Attributes {
    // usize storage provides the alignment required by the opaque native list.
    storage: Vec<usize>,
}
impl Attributes {
    fn new(count: u32) -> Result<Self> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes);
        }
        ensure!(bytes > 0, "InitializeProcThreadAttributeList: no size");
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        check(
            unsafe {
                InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut bytes)
            },
            "InitializeProcThreadAttributeList",
        )?;
        Ok(Self { storage })
    }
    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}

// Match Windows CRT argv parsing, including trailing backslashes before the
// closing quote. Never interpolate shell text into a second shell command.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
        } else {
            out.extend(std::iter::repeat_n('\\', slashes));
        }
        slashes = 0;
        out.push(ch);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// Grant write access to these roots ahead of any command (see
/// `tools::bash::prime_sandbox_caches`); failures are left for the command to report.
pub fn prime(roots: &[PathBuf]) {
    // One at a time, so a root that can't be granted doesn't stop the rest.
    for root in roots {
        let _ = grant_roots(std::slice::from_ref(root));
    }
}

/// Grant every existing writable root its restricting SID; returns the SIDs.
/// Missing paths are skipped: the command couldn't create them anyway.
fn grant_roots(writable: &[PathBuf]) -> Result<Vec<Sid>> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in writable {
        if std::fs::symlink_metadata(path).is_err() {
            continue;
        }
        // Canonicalize before touching ACLs: grant the real target, not a
        // junction, and derive one SID per real location. dunce, not
        // crate::util: tests/windows_sandbox.rs compiles this file on its own.
        let path = dunce::canonicalize(path)
            .with_context(|| format!("sandbox path {}", path.display()))?;
        if !roots.contains(&path) {
            roots.push(path);
        }
    }
    roots
        .iter()
        .map(|root| {
            let sid = Sid::for_root(root);
            grant_write(root, &sid)
                .with_context(|| format!("granting sandbox write access to {}", root.display()))?;
            Ok(sid)
        })
        .collect()
}

pub fn run(request: &Request) -> Result<u32> {
    run_with_timeout(request, INFINITE)
}

fn run_with_timeout(request: &Request, timeout: u32) -> Result<u32> {
    let name = request
        .program
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let system_bash = name.eq_ignore_ascii_case("bash.exe")
        && request
            .program
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|p| {
                p.eq_ignore_ascii_case("System32") || p.eq_ignore_ascii_case("Sysnative")
            });
    ensure!(
        !name.eq_ignore_ascii_case("wsl.exe") && !system_bash,
        "WSL shells cannot be contained by the Windows sandbox; use Git Bash or PowerShell"
    );
    let parent = Handle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, request.parent_pid) });
    ensure!(
        !parent.0.is_null(),
        "OpenProcess(parent): {}",
        std::io::Error::last_os_error()
    );
    let roots = grant_roots(&request.writable)?;
    // Unique to this launch: the default DACL grants it, so only this
    // launch's processes can write to each other's objects through it.
    let launch = Sid::random();
    // Reported only if the child then fails to start.
    let desktop = DesktopGrant::new(&launch);

    let mut base = null_mut();
    check(
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                // The restricted token's handle gets these same rights;
                // ADJUST_DEFAULT is needed to set its default DACL.
                TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
                &mut base,
            )
        },
        "OpenProcessToken",
    )?;
    let base = Handle(base);
    let mut restricting: Vec<Sid> = roots;
    restricting.push(Sid(launch.0.clone()));
    restricting.extend(logon_sid(base.0)?);
    restricting.push(Sid::well_known(WinWorldSid)?);
    restricting.push(Sid::well_known(WinRestrictedCodeSid)?);
    let entries: Vec<SID_AND_ATTRIBUTES> = restricting
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES {
            Sid: sid.ptr(),
            Attributes: 0,
        })
        .collect();
    let mut token = null_mut();
    check(
        unsafe {
            CreateRestrictedToken(
                base.0,
                DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
                0,
                null(),
                0,
                null(),
                entries.len() as u32,
                entries.as_ptr(),
                &mut token,
            )
        },
        "CreateRestrictedToken",
    )?;
    let token = Handle(token);
    let user = token_info(base.0, TokenUser)?;
    let user = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    allow_in_default_dacl(token.0, &[user, launch.ptr()])?;

    let job = Handle(unsafe { CreateJobObjectW(null(), null()) });
    ensure!(
        !job.0.is_null(),
        "CreateJobObjectW: {}",
        std::io::Error::last_os_error()
    );
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    check(
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        },
        "SetInformationJobObject",
    )?;

    let mut attrs = Attributes::new(1)?;
    let mut si: STARTUPINFOEXW = unsafe { zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    let duplicate = |kind: STD_HANDLE| -> Result<Handle> {
        let mut handle = null_mut();
        check(
            unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    GetStdHandle(kind),
                    GetCurrentProcess(),
                    &mut handle,
                    0,
                    1,
                    DUPLICATE_SAME_ACCESS,
                )
            },
            &format!("DuplicateHandle(std {kind:#x})"),
        )?;
        Ok(Handle(handle))
    };
    let stdin = duplicate(STD_INPUT_HANDLE)?;
    let stdout = duplicate(STD_OUTPUT_HANDLE)?;
    let stderr = duplicate(STD_ERROR_HANDLE)?;
    si.StartupInfo.hStdInput = stdin.0;
    si.StartupInfo.hStdOutput = stdout.0;
    si.StartupInfo.hStdError = stderr.0;
    let mut handles = [stdin.0, stdout.0, stderr.0];
    check(
        unsafe {
            UpdateProcThreadAttribute(
                attrs.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr().cast(),
                size_of_val(&handles),
                null_mut(),
                null(),
            )
        },
        "UpdateProcThreadAttribute(HANDLE_LIST)",
    )?;
    si.lpAttributeList = attrs.ptr();

    // cmd.exe mistakes a forward slash in its own path for a switch
    // ("The syntax of the command is incorrect").
    let program_text = request.program.to_string_lossy().replace('/', "\\");
    let program = wide(&program_text)?;
    let cmdline = if name.eq_ignore_ascii_case("cmd.exe") {
        // cmd does not parse argv using CRT quote/backslash rules. /S strips
        // exactly the outside quotes after /C, preserving the shell text inside.
        ensure!(request.args.len() >= 2, "missing cmd script");
        let flags = &request.args[..request.args.len() - 1];
        ensure!(
            flags.iter().all(|s| ["/D", "/S", "/C", "/Q"]
                .iter()
                .any(|f| s.eq_ignore_ascii_case(f)))
                && flags.last().is_some_and(|s| s.eq_ignore_ascii_case("/C")),
            "unsupported cmd launch flags"
        );
        format!(
            "{} /S {} \"{}\"",
            quote(&program_text),
            flags.join(" "),
            request.args.last().unwrap()
        )
    } else {
        std::iter::once(program_text.clone())
            .chain(request.args.iter().cloned())
            .map(|a| quote(&a))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut cmdline = wide(cmdline)?;
    let cwd = wide(&request.cwd)?;
    let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
    check(
        unsafe {
            CreateProcessAsUserW(
                token.0,
                program.as_ptr(),
                cmdline.as_mut_ptr(),
                null(),
                null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW,
                null(),
                cwd.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            )
        },
        "CreateProcessAsUserW",
    )?;
    let process = Handle(pi.hProcess);
    let thread = Handle(pi.hThread);
    // Until assignment succeeds the child is suspended. Every failure kills it;
    // no uncontained process is ever allowed to execute user code.
    if let Err(e) = check(
        unsafe { AssignProcessToJobObject(job.0, process.0) },
        "AssignProcessToJobObject",
    ) {
        unsafe {
            TerminateProcess(process.0, 1);
        }
        return Err(e);
    }
    ensure!(
        unsafe { ResumeThread(thread.0) } != u32::MAX,
        "ResumeThread: {}",
        std::io::Error::last_os_error()
    );
    let waiting = [process.0, parent.0];
    let status = unsafe { WaitForMultipleObjects(2, waiting.as_ptr(), 0, timeout) };
    ensure!(
        status == WAIT_OBJECT_0,
        "WaitForMultipleObjects: sandbox parent exited or command timed out ({status:#x})"
    );
    let mut code = 1;
    check(
        unsafe { GetExitCodeProcess(process.0, &mut code) },
        "GetExitCodeProcess",
    )?;
    // NTSTATUS/HRESULT-style exit codes (crashes, failed process
    // initialization) are otherwise silent.
    if code >= 0x8000_0000 {
        let note = desktop
            .as_ref()
            .err()
            .map(|e| format!(" (desktop access: {e:#})"))
            .unwrap_or_default();
        eprintln!(
            "Windows sandbox: {} exited with status {code:#x}{note}",
            request.program.display()
        );
    }
    // Kill remaining descendants before revoking the desktop grant.
    check(
        unsafe { TerminateJobObject(job.0, code) },
        "TerminateJobObject",
    )?;
    drop(job);
    drop(desktop);
    Ok(code)
}

/// Probe the actual launch mechanism once, rather than merely checking the OS.
/// Keep the probe entirely in a private temp directory.
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("chanto-sandbox-probe-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<bool> {
            std::fs::create_dir(&dir)?;
            // A pre-existing file proves grants reach existing descendants,
            // not only files the sandboxed command creates.
            std::fs::write(dir.join("existing.txt"), "ok")?;
            let system = std::env::var_os("SystemRoot").context("SystemRoot unavailable")?;
            let request = Request {
                program: PathBuf::from(system).join("System32/cmd.exe"),
                args: vec![
                    "/D".into(),
                    "/C".into(),
                    "type existing.txt>probe.txt 2>nul".into(),
                ],
                cwd: dir.clone(),
                writable: vec![dir.clone()],
                parent_pid: std::process::id(),
            };
            Ok(run_with_timeout(&request, 10_000)? == 0
                && std::fs::read_to_string(dir.join("probe.txt")).is_ok_and(|s| s.trim() == "ok"))
        })();
        let _ = std::fs::remove_dir_all(dir);
        result.unwrap_or(false)
    })
}

pub fn dispatch(arg: Option<&str>) -> ! {
    let result = (|| -> Result<u32> {
        let request: Request =
            serde_json::from_str(arg.context("missing sandbox launch request")?)?;
        run(&request)
    })();
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Windows sandbox: {e:#}");
            1
        }
    };
    std::process::exit(code as i32)
}
