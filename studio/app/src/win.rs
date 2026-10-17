//! Windows integration (DESIGN.md section 10): the console policy for GUI
//! launches, the per-user file association, start at login, the
//! notification identity, the VC++ runtime check, running the VC++ installer
//! once its signature checks out, the clipboard and error boxes. The registry
//! values themselves are in `winvalues.rs`.

// Without the GUI this module is only built for its tests.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::{anyhow, Context as _};
use windows_sys::Win32::Foundation::{
    CloseHandle, GlobalFree, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE,
    WAIT_OBJECT_0, WIN32_ERROR,
};
use windows_sys::Win32::Security::Cryptography::{
    CertGetNameStringW, CERT_NAME_SIMPLE_DISPLAY_TYPE,
};
use windows_sys::Win32::Security::WinTrust::{
    WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, FILE_SHARE_READ, VS_FIXEDFILEINFO,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteKeyW, RegDeleteTreeW, RegGetValueW,
    RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC,
    SEE_MASK_NOCLOSEPROCESS, SHCNE_ASSOCCHANGED, SHCNF_FLUSH, SHELLEXECUTEINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, SW_SHOWNORMAL,
};

use crate::winvalues::{self, CLASSES, PROG_ID, RUN_KEY, RUN_NAME};

/// A NUL-terminated UTF-16 string.
fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// What a GUI launch does with the console it may have been given.
#[cfg(feature = "gui")]
pub enum GuiLaunch {
    /// Run the GUI in this process.
    Here,
    /// A detached copy (same arguments plus `--gui-child`) was started; exit 0
    /// so the shell prompt returns.
    Relaunched,
}

/// No console attached: run here. Launched from a terminal: relaunch detached
/// (`DETACHED_PROCESS`). Older Windows, where Explorer still allocates a
/// console: if `GetConsoleProcessList` returns 1, `FreeConsole()` and run here.
#[cfg(feature = "gui")]
pub fn prepare_gui_launch(gui_child: bool) -> anyhow::Result<GuiLaunch> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};
    use windows_sys::Win32::System::Console::{
        FreeConsole, GetConsoleProcessList, GetConsoleWindow,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    if gui_child {
        return Ok(GuiLaunch::Here);
    }
    // SAFETY: plain queries of this process's console.
    if unsafe { GetConsoleWindow() }.is_null() {
        return Ok(GuiLaunch::Here);
    }
    let mut pids = [0u32; 4];
    // SAFETY: the buffer holds the count we pass.
    let attached = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if attached == 1 {
        // Explorer gave this process a console of its own (Windows before
        // 24H2 ignores consoleAllocationPolicy): drop it and run here.
        // SAFETY: detaches from the console; std treats the closed handles as
        // successful no-op writes.
        unsafe { FreeConsole() };
        return Ok(GuiLaunch::Here);
    }
    let exe = std::env::current_exe().context("cannot find jai.exe")?;
    Command::new(exe)
        .arg("--gui-child")
        .args(std::env::args_os().skip(1))
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start JAI Studio")?;
    Ok(GuiLaunch::Relaunched)
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegCreateKeyExW or RegOpenKeyExW.
        unsafe { RegCloseKey(self.0) };
    }
}

fn check(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// `check`, with "not found" counted as done.
fn check_gone(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(code)
    }
}

/// Creates or opens `HKCU\<path>` for writing.
fn create_key(path: &str) -> io::Result<Key> {
    let path_w = wide(path);
    let mut key: HKEY = null_mut();
    // SAFETY: valid NUL-terminated strings and out pointer.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path_w.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            null(),
            &mut key,
            null_mut(),
        )
    })?;
    Ok(Key(key))
}

/// Sets a REG_SZ value; an empty name sets the (Default) value.
fn set_string(key: &Key, name: &str, data: &str) -> io::Result<()> {
    let name_w = wide(name);
    let data_w = wide(data);
    let name_ptr = if name.is_empty() {
        null()
    } else {
        name_w.as_ptr()
    };
    // SAFETY: data_w lives for the call; the size includes the NUL.
    check(unsafe {
        RegSetValueExW(
            key.0,
            name_ptr,
            0,
            REG_SZ,
            data_w.as_ptr().cast::<u8>(),
            (data_w.len() * 2) as u32,
        )
    })
}

/// A REG_SZ value under HKCU, if it exists.
fn get_string(path: &str, name: &str) -> Option<String> {
    let path_w = wide(path);
    let name_w = wide(name);
    let name_ptr = if name.is_empty() {
        null()
    } else {
        name_w.as_ptr()
    };
    let mut size = 0u32;
    // SAFETY: a size query with a null buffer.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path_w.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        )
    };
    if code != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    // SAFETY: buf holds `size` bytes.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path_w.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            null_mut(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut size,
        )
    };
    if code != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Deletes a value under HKCU; an empty name deletes the (Default) value.
fn delete_value(path: &str, name: &str) -> io::Result<()> {
    let path_w = wide(path);
    let name_w = wide(name);
    let name_ptr = if name.is_empty() {
        null()
    } else {
        name_w.as_ptr()
    };
    // SAFETY: valid NUL-terminated strings.
    check_gone(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, path_w.as_ptr(), name_ptr) })
}

/// Deletes `HKCU\<path>` and everything under it.
fn delete_tree(path: &str) -> io::Result<()> {
    let path_w = wide(path);
    // SAFETY: valid NUL-terminated string. RegDeleteTreeW leaves the key
    // itself, which RegDeleteKeyW then removes.
    check_gone(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path_w.as_ptr()) })?;
    check_gone(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path_w.as_ptr()) })
}

/// Deletes `HKCU\<path>` when it has no values and no subkeys left.
fn delete_key_if_empty(path: &str) -> io::Result<()> {
    let path_w = wide(path);
    let mut key: HKEY = null_mut();
    // SAFETY: valid string and out pointer.
    let code = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path_w.as_ptr(), 0, KEY_READ, &mut key) };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(code)?;
    let key = Key(key);
    let (mut subkeys, mut values) = (0u32, 0u32);
    // SAFETY: only the two counts are asked for.
    check(unsafe {
        RegQueryInfoKeyW(
            key.0,
            null_mut(),
            null_mut(),
            null(),
            &mut subkeys,
            null_mut(),
            null_mut(),
            &mut values,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    })?;
    drop(key);
    if subkeys == 0 && values == 0 {
        // SAFETY: valid NUL-terminated string.
        check_gone(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, path_w.as_ptr()) })?;
    }
    Ok(())
}

/// This exe's full path as text.
fn exe_path() -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("cannot find jai.exe")?;
    Ok(exe.to_string_lossy().into_owned())
}

/// Writes or removes the per-user `.jai` association under
/// `HKCU\Software\Classes`, then calls `SHChangeNotify(SHCNE_ASSOCCHANGED,
/// SHCNF_FLUSH)`.
pub fn set_file_association(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        let exe = exe_path()?;
        for v in winvalues::association_values(&exe) {
            let key =
                create_key(&v.key).with_context(|| format!("cannot write HKCU\\{}", v.key))?;
            set_string(&key, &v.name, &v.data)
                .with_context(|| format!("cannot write HKCU\\{}", v.key))?;
        }
    } else {
        let ext = format!(r"{CLASSES}\.jai");
        let with = format!(r"{ext}\OpenWithProgids");
        delete_tree(&format!(r"{CLASSES}\{PROG_ID}")).context("cannot remove JAI.Document")?;
        delete_value(&with, PROG_ID).context("cannot remove the .jai OpenWithProgids entry")?;
        if get_string(&ext, "").as_deref() == Some(PROG_ID) {
            delete_value(&ext, "").context("cannot remove the .jai default")?;
        }
        delete_key_if_empty(&with)?;
        delete_key_if_empty(&ext)?;
    }
    // SAFETY: SHCNE_ASSOCCHANGED takes no items.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_FLUSH, null(), null()) };
    Ok(())
}

/// Registers JAI Studio's AppUserModelID per user, under
/// `HKCU\Software\Classes\AppUserModelId\ai.gnist.jai`, with its name and
/// `icon`. tauri-plugin-notification shows toasts under that id, and Windows
/// drops the toasts of an unpackaged program whose id is not registered (or
/// on a Start menu shortcut, which Studio has none of).
pub fn register_app_id(icon: &Path) -> anyhow::Result<()> {
    for v in winvalues::app_id_values(&icon.to_string_lossy()) {
        let key = create_key(&v.key).with_context(|| format!("cannot write HKCU\\{}", v.key))?;
        set_string(&key, &v.name, &v.data)
            .with_context(|| format!("cannot write HKCU\\{}", v.key))?;
    }
    Ok(())
}

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `JAI` =
/// `"<exe>" tray`.
pub fn set_start_at_login(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        let exe = exe_path()?;
        let key = create_key(RUN_KEY).context("cannot open the Run key")?;
        set_string(&key, RUN_NAME, &winvalues::run_value(&exe))
            .context("cannot write the Run value")?;
    } else {
        delete_value(RUN_KEY, RUN_NAME).context("cannot remove the Run value")?;
    }
    Ok(())
}

/// `C:\Windows\System32`.
fn system_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 512];
    // SAFETY: the buffer length is passed.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// The high word pair (major, minor) of a file's version resource.
fn file_version_ms(path: &Path) -> Option<u32> {
    let path_w = wide(path);
    let mut handle = 0u32;
    // SAFETY: size query on a NUL-terminated path.
    let size = unsafe { GetFileVersionInfoSizeW(path_w.as_ptr(), &mut handle) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    // SAFETY: data holds `size` bytes.
    if unsafe { GetFileVersionInfoW(path_w.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let root = wide("\\");
    let mut info: *mut c_void = null_mut();
    let mut len = 0u32;
    // SAFETY: VerQueryValueW points `info` into `data`, which outlives it.
    let ok = unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) };
    if ok == 0 || info.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }
    // SAFETY: checked non-null and large enough above.
    let fixed = unsafe { std::ptr::read_unaligned(info.cast::<VS_FIXEDFILEINFO>()) };
    Some(fixed.dwFileVersionMS)
}

/// `System32\msvcp140.dll` is version 14.40 or newer.
pub fn vc_runtime_ok() -> bool {
    system_dir()
        .and_then(|dir| file_version_ms(&dir.join("msvcp140.dll")))
        .is_some_and(winvalues::vc_version_ok)
}

/// Who signs the VC++ redistributable.
const MICROSOFT: &str = "Microsoft Corporation";

/// How a program started through the shell ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellRun {
    /// The user said no to Windows' permission (UAC) prompt.
    Declined,
    /// It ran and exited with this code.
    Exited(u32),
    /// It started, but Windows gave no process to wait for.
    Untracked,
}

/// Starts a downloaded Microsoft installer, but only when its Authenticode
/// signature is valid (WinVerifyTrust) and its signer is Microsoft
/// Corporation, and waits for it to exit. The file stays open from the check
/// until it has started, readable but not writable or deletable by anyone
/// else, so it cannot be swapped in between.
pub fn run_microsoft_installer(path: &Path) -> anyhow::Result<ShellRun> {
    use std::os::windows::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let signer = authenticode_signer(&file, path)
        .with_context(|| format!("the installer was not started: {}", path.display()))?;
    if signer != MICROSOFT {
        anyhow::bail!("the installer is signed by {signer:?}, not {MICROSOFT}; it was not started");
    }
    let started = shell_start(path, None, SW_SHOWNORMAL)?;
    drop(file);
    match started {
        ShellStart::Declined => Ok(ShellRun::Declined),
        ShellStart::Untracked => Ok(ShellRun::Untracked),
        ShellStart::Started(process) => Ok(ShellRun::Exited(process.wait()?)),
    }
}

/// The signer named in a file's valid Authenticode signature: the simple
/// display name of the signing certificate, whose chain WinVerifyTrust has
/// checked up to a trusted root.
fn authenticode_signer(file: &File, path: &Path) -> anyhow::Result<String> {
    use std::os::windows::io::AsRawHandle as _;
    let path_w = wide(path);
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: path_w.as_ptr(),
        hFile: file.as_raw_handle() as HANDLE,
        pgKnownSubject: null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: `data` points at `file_info`, which points at `path_w` and the
    // open file; all outlive both calls. The second call frees the state the
    // first one kept.
    let status = unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    let signer = if status == 0 {
        // SAFETY: a successful verification left its state in hWVTStateData.
        unsafe { signer_name(data.hWVTStateData) }
    } else {
        Err(anyhow!("{}", trust_error(status)))
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: as above.
    unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    signer
}

/// The first signer's certificate name from WinVerifyTrust's state.
///
/// # Safety
/// `state` must be the `hWVTStateData` of a successful verification that has
/// not been closed yet.
unsafe fn signer_name(state: HANDLE) -> anyhow::Result<String> {
    let provider = WTHelperProvDataFromStateData(state);
    if provider.is_null() {
        anyhow::bail!("the signature has no signer");
    }
    let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
    if signer.is_null() {
        anyhow::bail!("the signature has no signer");
    }
    let cert = WTHelperGetProvCertFromChain(signer, 0);
    if cert.is_null() || (*cert).pCert.is_null() {
        anyhow::bail!("the signature has no certificate");
    }
    let context = (*cert).pCert;
    let len = CertGetNameStringW(
        context,
        CERT_NAME_SIMPLE_DISPLAY_TYPE,
        0,
        null(),
        null_mut(),
        0,
    );
    if len <= 1 {
        anyhow::bail!("the signing certificate has no name");
    }
    let mut name = vec![0u16; len as usize];
    let written = CertGetNameStringW(
        context,
        CERT_NAME_SIMPLE_DISPLAY_TYPE,
        0,
        null(),
        name.as_mut_ptr(),
        len,
    ) as usize;
    let end = name[..written.min(name.len())]
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(written.min(name.len()));
    Ok(String::from_utf16_lossy(&name[..end]))
}

/// What a WinVerifyTrust failure means.
fn trust_error(status: i32) -> String {
    match status as u32 {
        0x800B_0100 => "it is not signed".into(),
        0x8009_6010 => "its signature does not match its contents".into(),
        0x800B_0109 | 0x800B_010A => "its signature does not lead to a trusted root".into(),
        0x800B_0101 => "its signing certificate has expired".into(),
        0x800B_010C => "its signing certificate was revoked".into(),
        0x800B_0004 | 0x800B_0111 => "Windows does not trust its signer".into(),
        other => format!("its signature cannot be checked (error {other:#010x})"),
    }
}

/// What [`shell_start`] started.
enum ShellStart {
    Declined,
    Started(Process),
    Untracked,
}

/// A process handle, closed when dropped.
struct Process(HANDLE);

impl Process {
    /// Waits for the process to exit; its exit code.
    fn wait(&self) -> anyhow::Result<u32> {
        // SAFETY: a process handle this struct owns, which ShellExecuteExW
        // opened with the rights to wait for it and read its exit code.
        unsafe {
            if WaitForSingleObject(self.0, INFINITE) != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error()).context("cannot wait for the program");
            }
            let mut code = 0u32;
            if GetExitCodeProcess(self.0, &mut code) == 0 {
                return Err(io::Error::last_os_error())
                    .context("cannot read the program's exit code");
            }
            Ok(code)
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: closes the handle this struct owns, once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Opens a file with its default verb, as Explorer does (an installer shows
/// the UAC prompt), and returns its process to wait for.
fn shell_start(path: &Path, params: Option<&str>, show: i32) -> anyhow::Result<ShellStart> {
    let verb = wide("open");
    let file = wide(path);
    let params = params.map(wide);
    // SAFETY: all zeros is a valid SHELLEXECUTEINFOW: no flags, null pointers.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    // No error boxes from the shell: the caller reports failures.
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ref().map_or(null(), |p| p.as_ptr());
    info.nShow = show;
    // SAFETY: `info` points at NUL-terminated strings that outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Ok(ShellStart::Declined);
        }
        return Err(err).with_context(|| format!("cannot start {}", path.display()));
    }
    Ok(if info.hProcess.is_null() {
        ShellStart::Untracked
    } else {
        ShellStart::Started(Process(info.hProcess))
    })
}

/// Puts text on the clipboard.
pub fn copy_text(text: &str) -> anyhow::Result<()> {
    const CF_UNICODETEXT: u32 = 13;
    let data: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let bytes = data.len() * 2;
    // SAFETY: the clipboard is opened, filled with a moveable global block
    // that the system owns once SetClipboardData succeeds, and closed.
    unsafe {
        if OpenClipboard(null_mut()) == 0 {
            anyhow::bail!("the clipboard is busy");
        }
        let result = (|| {
            EmptyClipboard();
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if mem.is_null() {
                anyhow::bail!("out of memory");
            }
            let ptr = GlobalLock(mem).cast::<u16>();
            if ptr.is_null() {
                GlobalFree(mem);
                anyhow::bail!("out of memory");
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            GlobalUnlock(mem);
            if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                GlobalFree(mem);
                anyhow::bail!("cannot set the clipboard");
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

/// A modal error box, for failures before the window exists (a GUI launch
/// has no console to print to).
pub fn error_box(title: &str, message: &str) {
    let title = wide(title);
    let message = wide(message);
    // SAFETY: valid NUL-terminated strings; no owner window.
    unsafe {
        MessageBoxW(
            null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry helpers on a throwaway key, removed at the end.
    #[test]
    fn registry_round_trip() {
        let base = format!(r"Software\ai.gnist.jai.test-{}", std::process::id());
        let sub = format!(r"{base}\shell\open\command");
        let key = create_key(&sub).unwrap();
        set_string(&key, "", r#""C:\Program Files\JAI\jai.exe" "%1""#).unwrap();
        set_string(&key, "Empty", "").unwrap();
        drop(key);
        assert_eq!(
            get_string(&sub, "").as_deref(),
            Some(r#""C:\Program Files\JAI\jai.exe" "%1""#)
        );
        assert_eq!(get_string(&sub, "Empty").as_deref(), Some(""));
        delete_value(&sub, "Empty").unwrap();
        assert_eq!(get_string(&sub, "Empty"), None);
        delete_value(&sub, "Empty").unwrap();
        // Still holds its default value, so it stays.
        delete_key_if_empty(&sub).unwrap();
        assert!(get_string(&sub, "").is_some());
        delete_value(&sub, "").unwrap();
        delete_key_if_empty(&sub).unwrap();
        assert_eq!(get_string(&sub, ""), None);
        delete_tree(&base).unwrap();
        delete_tree(&base).unwrap();
        delete_key_if_empty(&base).unwrap();
    }

    /// A program started through the shell is waited for, with its exit
    /// code, as the VC++ installer is.
    #[test]
    fn shell_started_programs_are_waited_for() {
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        let cmd = system_dir().unwrap().join("cmd.exe");
        match shell_start(&cmd, Some("/d /c exit 3010"), SW_HIDE).unwrap() {
            ShellStart::Started(process) => assert_eq!(process.wait().unwrap(), 3010),
            ShellStart::Declined => panic!("no prompt is shown for cmd.exe"),
            ShellStart::Untracked => panic!("an exe always gives a process"),
        }
        let missing = system_dir().unwrap().join("no-such-program-jai.exe");
        assert!(shell_start(&missing, None, SW_HIDE).is_err());
    }

    #[test]
    fn signatures() {
        // This test program is not signed.
        let exe = std::env::current_exe().unwrap();
        let file = File::open(&exe).unwrap();
        let e = authenticode_signer(&file, &exe).unwrap_err();
        assert!(e.to_string().contains("not signed"), "{e}");
        assert!(run_microsoft_installer(&exe)
            .unwrap_err()
            .to_string()
            .contains("not started"));
        // Microsoft's own programs, where a runner has them with an embedded
        // signature (system files are often signed in catalogs instead). Not
        // all of them name "Microsoft Corporation": a System32 msvcp140.dll
        // may be signed by "Microsoft Windows Software Compatibility
        // Publisher". The installer check itself accepts only the former.
        let candidates = [
            system_dir().unwrap().join("msvcp140.dll"),
            PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
            PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        ];
        for path in candidates.iter().filter(|p| p.is_file()) {
            let file = File::open(path).unwrap();
            if let Ok(signer) = authenticode_signer(&file, path) {
                assert!(
                    signer.starts_with("Microsoft "),
                    "{}: {signer}",
                    path.display()
                );
            }
        }
        assert!(trust_error(0x800B_0100u32 as i32).contains("not signed"));
        assert!(trust_error(1).contains("0x00000001"));
    }

    #[test]
    fn version_resources() {
        let dir = system_dir().unwrap();
        let ms = file_version_ms(&dir.join("kernel32.dll")).unwrap();
        assert!(ms >> 16 >= 6, "{ms:#x}");
        assert_eq!(file_version_ms(&dir.join("no-such-file.dll")), None);
        // Either answer is fine; it must not fail.
        let _ = vc_runtime_ok();
    }
}
