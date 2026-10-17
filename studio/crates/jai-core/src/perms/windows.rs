//! Windows: private means owned by the user, SYSTEM or Administrators, with
//! no allow entry in the access list for anyone else. Studio's folders get a
//! protected access list (no inheritance from above) that grants full
//! control to exactly those three, inherited by everything inside.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorDacl, GetTokenInformation, IsValidSid, TokenUser,
    WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid, WinCreatorOwnerSid, WinLocalSystemSid,
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{Access, Check};

/// ACE types (winnt.h) laid out as header, mask, SID.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0x0;
const ACCESS_ALLOWED_CALLBACK_ACE_TYPE: u8 = 0x9;
/// The other allow types (compound, object, callback object) keep their SID
/// elsewhere; they never occur on files, so they count as foreign.
const OTHER_ALLOW_TYPES: [u8; 3] = [0x4, 0x5, 0xB];
/// An ACE header and mask (8 bytes) plus the smallest SID (8 bytes).
const MIN_ACE_SIZE: usize = 16;

/// Rights that change a file or folder, what it contains, or who may use it.
const WRITE_RIGHTS: u32 = 0x0000_0002 // FILE_WRITE_DATA, FILE_ADD_FILE
    | 0x0000_0004 // FILE_APPEND_DATA, FILE_ADD_SUBDIRECTORY
    | 0x0000_0010 // FILE_WRITE_EA
    | 0x0000_0040 // FILE_DELETE_CHILD
    | 0x0000_0100 // FILE_WRITE_ATTRIBUTES
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE

pub(super) fn inspect(path: &Path, check: Check) -> io::Result<Access> {
    let trusted = Trusted::new()?;
    let wide = wide(path)?;
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: a NUL-terminated path and valid out pointers. The owner and the
    // access list point into `sd`, which `_sd` frees after their last use.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    let _sd = Local(sd);
    if owner.is_null() || !trusted.owns(owner) {
        return Ok(Access::Shared("it belongs to another account".into()));
    }
    if dacl.is_null() {
        return Ok(Access::Shared(
            "it has no access list, so every account can open it".into(),
        ));
    }
    // SAFETY: `dacl` is the access list in `sd`, alive until `_sd` drops.
    unsafe { scan(dacl, &trusted, check) }
}

/// The first allow entry for an account outside `trusted` that matters for
/// `check`, as the reason; Private when there is none.
///
/// # Safety
/// `dacl` must point to a valid access list.
unsafe fn scan(dacl: *const ACL, trusted: &Trusted, check: Check) -> io::Result<Access> {
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `info` is an ACL_SIZE_INFORMATION of the size given.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `index` is below the entry count.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        let header = unsafe { std::ptr::read_unaligned(ace as *const ACE_HEADER) };
        let (mask, who) = match header.AceType {
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => {
                if usize::from(header.AceSize) < MIN_ACE_SIZE {
                    return Ok(Access::Shared("its access list is damaged".into()));
                }
                // SAFETY: the ACE is at least MIN_ACE_SIZE bytes: the mask
                // follows the 4-byte header and the SID the mask.
                let (mask, sid) = unsafe {
                    let bytes = ace as *mut u8;
                    (
                        std::ptr::read_unaligned(bytes.add(4) as *const u32),
                        bytes.add(8) as PSID,
                    )
                };
                // SAFETY: `sid` points into the ACE.
                if unsafe { IsValidSid(sid) } == 0 {
                    return Ok(Access::Shared("its access list is damaged".into()));
                }
                if trusted.may_be_granted(sid) {
                    continue;
                }
                (mask, account_name(sid))
            }
            kind if OTHER_ALLOW_TYPES.contains(&kind) => (u32::MAX, "another account".into()),
            // Deny entries take rights away; audit entries and labels grant
            // nothing.
            _ => continue,
        };
        let matters = match check {
            Check::Any => mask != 0,
            Check::Write => mask & WRITE_RIGHTS != 0,
        };
        if matters {
            let what = if mask & WRITE_RIGHTS != 0 {
                "change"
            } else {
                "read"
            };
            return Ok(Access::Shared(format!("{who} may {what} it")));
        }
    }
    Ok(Access::Private)
}

/// Creates one folder with the private access list. Its parent must exist.
pub(super) fn create_private(path: &Path) -> io::Result<()> {
    let sd = PrivateDescriptor::new()?;
    let wide = wide(path)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 .0,
        bInheritHandle: 0,
    };
    // SAFETY: a NUL-terminated path and attributes whose descriptor outlives
    // the call.
    if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Folders inside the private root inherit its access list.
pub(super) fn create_dir_all(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Replaces the access list with the private one. Windows passes the
/// inheritable entries on to everything inside.
pub(super) fn make_private(path: &Path) -> io::Result<()> {
    let sd = PrivateDescriptor::new()?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = null_mut();
    // SAFETY: `sd` is a valid descriptor and the out pointers are valid; the
    // access list points into `sd`, which lives until the end of the function.
    let ok = unsafe { GetSecurityDescriptorDacl(sd.0 .0, &mut present, &mut dacl, &mut defaulted) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if present == 0 || dacl.is_null() {
        return Err(io::Error::other("the private access list is empty"));
    }
    let wide = wide(path)?;
    // SAFETY: a NUL-terminated path and the access list from `sd`.
    let err = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    if err != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    Ok(())
}

/// A security descriptor whose protected access list grants full control to
/// the user, SYSTEM and Administrators, inherited by files and folders.
struct PrivateDescriptor(Local);

impl PrivateDescriptor {
    fn new() -> io::Result<PrivateDescriptor> {
        let user = Sid::current_user()?.to_string_sid()?;
        let sddl = format!("D:P(A;OICI;FA;;;{user})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: a NUL-terminated string and a valid out pointer; the
        // descriptor is freed by `Local`.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(PrivateDescriptor(Local(sd)))
    }
}

/// The accounts a private path may grant access to.
struct Trusted {
    /// The user, SYSTEM and Administrators: also the acceptable owners.
    owners: [Sid; 3],
    /// CREATOR OWNER and OWNER RIGHTS stand for the owner.
    placeholders: [Sid; 2],
}

impl Trusted {
    fn new() -> io::Result<Trusted> {
        Ok(Trusted {
            owners: [
                Sid::current_user()?,
                Sid::well_known(WinLocalSystemSid)?,
                Sid::well_known(WinBuiltinAdministratorsSid)?,
            ],
            placeholders: [
                Sid::well_known(WinCreatorOwnerSid)?,
                Sid::well_known(WinCreatorOwnerRightsSid)?,
            ],
        })
    }

    fn owns(&self, sid: PSID) -> bool {
        self.owners.iter().any(|s| s.equals(sid))
    }

    fn may_be_granted(&self, sid: PSID) -> bool {
        self.owns(sid) || self.placeholders.iter().any(|s| s.equals(sid))
    }
}

/// A SID in its own DWORD-aligned buffer.
struct Sid(Vec<u32>);

impl Sid {
    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> io::Result<Sid> {
        let mut buf = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
        let mut size = SECURITY_MAX_SID_SIZE;
        // SAFETY: the buffer holds SECURITY_MAX_SID_SIZE bytes, as `size` says.
        if unsafe { CreateWellKnownSid(kind, null_mut(), buf.as_mut_ptr().cast(), &mut size) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Sid(buf))
    }

    /// The account this process runs as.
    fn current_user() -> io::Result<Sid> {
        let mut token: HANDLE = null_mut();
        // SAFETY: the pseudo handle of this process and a valid out pointer;
        // the token handle is closed below.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let user = token_user(token);
        // SAFETY: the handle came from OpenProcessToken and is closed once.
        unsafe { CloseHandle(token) };
        user
    }

    fn psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    fn equals(&self, sid: PSID) -> bool {
        // SAFETY: both are valid SIDs.
        unsafe { EqualSid(self.psid(), sid) != 0 }
    }

    /// `S-1-5-21-…`
    fn to_string_sid(&self) -> io::Result<String> {
        string_sid(self.psid())
    }
}

fn token_user(token: HANDLE) -> io::Result<Sid> {
    let mut len = 0u32;
    // SAFETY: a size query without a buffer; it fails and sets `len`.
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 elements: TOKEN_USER holds a pointer.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: the buffer holds at least `len` bytes and is 8-aligned.
    let ok =
        unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the buffer starts with a TOKEN_USER whose SID points
    // into the buffer, which is alive while it is copied.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    // SAFETY: as above; the length comes from the SID itself.
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return Err(io::Error::other("the process token has no valid user"));
        }
        let size = GetLengthSid(sid) as usize;
        let mut copy = vec![0u32; size.div_ceil(4)];
        std::ptr::copy_nonoverlapping(sid as *const u8, copy.as_mut_ptr() as *mut u8, size);
        Ok(Sid(copy))
    }
}

fn string_sid(sid: PSID) -> io::Result<String> {
    let mut text: *mut u16 = null_mut();
    // SAFETY: a valid SID and out pointer; the string is freed by `Local`.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 || text.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _free = Local(text.cast());
    // SAFETY: the string is NUL-terminated.
    let len = unsafe { (0..).take_while(|&i| *text.add(i) != 0).count() };
    // SAFETY: `len` units precede the NUL.
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    String::from_utf16(units).map_err(io::Error::other)
}

/// A readable name for a SID in messages: the well-known groups that show up
/// on shared folders by name, others as `S-1-…`.
fn account_name(sid: PSID) -> String {
    let Ok(text) = string_sid(sid) else {
        return "another account".into();
    };
    let name = match text.as_str() {
        "S-1-1-0" => "Everyone",
        "S-1-5-11" => "Authenticated Users",
        "S-1-5-32-545" => "Users",
        "S-1-5-4" => "INTERACTIVE",
        "S-1-3-1" => "CREATOR GROUP",
        "S-1-15-2-1" => "ALL APPLICATION PACKAGES",
        _ => return format!("the account {text}"),
    };
    name.into()
}

/// A path as a NUL-terminated wide string.
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path contains a NUL character",
        ));
    }
    Ok(wide.into_iter().chain(Some(0)).collect())
}

/// Memory from LocalAlloc, freed on drop.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a function that allocates with
            // LocalAlloc, and is freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}
