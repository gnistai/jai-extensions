//! Windows process control: no console window, a kill-on-close Job object so
//! the engines die with jai, and TerminateProcess to stop.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::warn;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_LISTEN,
    TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

/// Time to exit after TerminateProcess before giving up.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// Without CREATE_NO_WINDOW a console window opens, because Studio has none.
pub(super) fn configure(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// TerminateProcess (through `start_kill`), then wait for the exit.
pub(super) async fn terminate(child: &mut Child, name: &str) -> Option<ExitStatus> {
    if let Err(err) = child.start_kill() {
        warn!("cannot terminate {name}: {err}");
    }
    match timeout(KILL_WAIT, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(err)) => {
            warn!("cannot wait for {name}: {err}");
            None
        }
        Err(_) => {
            warn!("{name} is still running 10 s after TerminateProcess");
            None
        }
    }
}

/// AF_INET (ws2def.h): IPv4 listeners only, as the engines bind IPv4.
const AF_INET: u32 = 2;

/// Whether process `pid` owns a TCP socket listening on exactly `addr`,
/// from the IPv4 listener table with owning process ids.
pub(super) fn listens_on(pid: u32, addr: SocketAddrV4) -> io::Result<bool> {
    let mut buf: Vec<u64> = Vec::new();
    let mut size = 0u32;
    // The table can grow between the size query and the read: retry.
    for _ in 0..8 {
        let ptr = if buf.is_empty() {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        // SAFETY: `ptr` is null with `size` 0, or a buffer of `size` bytes.
        let err = unsafe {
            GetExtendedTcpTable(ptr, &mut size, 0, AF_INET, TCP_TABLE_OWNER_PID_LISTENER, 0)
        };
        if err == NO_ERROR && !buf.is_empty() {
            return Ok(table_has(&buf, pid, addr));
        }
        if err != NO_ERROR && err != ERROR_INSUFFICIENT_BUFFER {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        // Room for what it asked for plus a few more rows.
        let bytes = size as usize + 16 * std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
        buf = vec![0u64; bytes.div_ceil(8)];
        size = u32::try_from(buf.len() * 8).unwrap_or(u32::MAX);
    }
    Err(io::Error::other("the TCP listener table kept changing"))
}

/// Looks through a MIB_TCPTABLE_OWNER_PID that GetExtendedTcpTable filled.
fn table_has(buf: &[u64], pid: u32, addr: SocketAddrV4) -> bool {
    let bytes = buf.len() * 8;
    let rows_at = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
    let row_size = std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
    // SAFETY: the buffer holds at least the entry count, which comes first.
    let count = unsafe { (*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID)).dwNumEntries } as usize;
    let count = count.min(bytes.saturating_sub(rows_at) / row_size);
    // SAFETY: `count` rows of MIB_TCPROW_OWNER_PID follow the count, inside
    // the buffer, which is 8-aligned.
    let rows = unsafe {
        std::slice::from_raw_parts(
            (buf.as_ptr() as *const u8).add(rows_at) as *const MIB_TCPROW_OWNER_PID,
            count,
        )
    };
    rows.iter().any(|row| {
        // Address and port are in network byte order; the port is the low
        // 16 bits.
        let ip = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes());
        let port = u16::from_be(row.dwLocalPort as u16);
        row.dwState == MIB_TCP_STATE_LISTEN as u32
            && row.dwOwningPid == pid
            && ip == *addr.ip()
            && port == addr.port()
    })
}

/// Puts the child in a new kill-on-close job right after spawn. On failure the
/// engine still runs, without that guarantee.
pub(super) fn contain(child: &Child, name: &str) -> Option<Job> {
    let handle = child.raw_handle()?;
    match Job::kill_on_close().and_then(|job| job.assign(handle).map(|()| job)) {
        Ok(job) => Some(job),
        Err(err) => {
            warn!("cannot put {name} in a job object; it may outlive jai: {err}");
            None
        }
    }
}

/// A Job object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE. Dropping it closes
/// the handle, which kills every process still in the job.
pub(crate) struct Job(HANDLE);

// SAFETY: a job handle is a kernel handle, usable from any thread; it is only
// closed once, in Drop.
unsafe impl Send for Job {}
// SAFETY: as above; &Job allows no access to the handle.
unsafe impl Sync for Job {}

impl Job {
    fn kill_on_close() -> io::Result<Job> {
        // SAFETY: no security attributes and no name.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer and size describe `info`, which outlives the call.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
        // SAFETY: both handles are valid: the job is ours, and the process
        // handle belongs to a Child that has not been waited for.
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed only here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
