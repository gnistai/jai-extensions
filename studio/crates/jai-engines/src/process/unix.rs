//! Linux process control: the engine gets its own process group and dies with
//! the thread that started it; stopping sends SIGTERM, then SIGKILL.

use std::io;
use std::net::SocketAddrV4;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::warn;

/// Time to exit after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// Time to exit after SIGKILL before giving up.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// A new process group (so a terminal's Ctrl+C reaches only jai, which stops
/// the engines itself) and, on Linux, `PR_SET_PDEATHSIG` = SIGKILL. The death
/// signal follows the spawning thread; Tokio's worker threads live as long as
/// the runtime.
pub(super) fn configure(cmd: &mut Command) {
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    {
        let parent = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        // SAFETY: the closure runs in the forked child before exec. It only
        // makes system calls and builds errors without allocating, which is
        // async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // The parent may have exited before the death signal was set.
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
}

/// SIGTERM to the engine's group; SIGKILL if it is still there after 3 s.
pub(super) async fn terminate(child: &mut Child, name: &str) -> Option<ExitStatus> {
    let Some(pid) = child.id() else {
        return child.try_wait().ok().flatten();
    };
    signal_group(pid, libc::SIGTERM);
    if let Ok(Ok(status)) = timeout(TERM_GRACE, child.wait()).await {
        return Some(status);
    }
    warn!(pid, "{name} ignored SIGTERM for 3 s; sending SIGKILL");
    signal_group(pid, libc::SIGKILL);
    match timeout(KILL_WAIT, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(err)) => {
            warn!(pid, "cannot wait for {name}: {err}");
            None
        }
        Err(_) => {
            warn!(pid, "{name} is still running 10 s after SIGKILL");
            None
        }
    }
}

/// Sends a signal to the engine's process group, or to the process alone if
/// that fails. Does nothing for pids that would address other processes.
pub(super) fn signal_group(pid: u32, signal: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // SAFETY: plain system calls. The engine leads its own group, so -pid
    // addresses the engine and anything it started.
    unsafe {
        if libc::kill(-pid, signal) != 0 {
            libc::kill(pid, signal);
        }
    }
}

/// Whether process `pid` has a TCP socket listening on exactly `addr`: an
/// IPv4 listener on that address and port in `/proc/<pid>/net/tcp` whose
/// inode is one of the process's open sockets in `/proc/<pid>/fd`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn listens_on(pid: u32, addr: SocketAddrV4) -> io::Result<bool> {
    let table = std::fs::read_to_string(format!("/proc/{pid}/net/tcp"))?;
    let inodes = listening_inodes(&table, addr);
    if inodes.is_empty() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd"))? {
        let Ok(entry) = entry else { continue };
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        if socket_inode(&target).is_some_and(|inode| inodes.contains(&inode)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Other Unix systems have no `/proc` to ask, and no engine builds.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) fn listens_on(_pid: u32, _addr: SocketAddrV4) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this system cannot tell which process listens on a port",
    ))
}

/// The inodes of the sockets in a `/proc/net/tcp` table that listen on
/// `addr`. Addresses there are hexadecimal in the kernel's byte order, ports
/// hexadecimal; state `0A` is LISTEN.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub(super) fn listening_inodes(table: &str, addr: SocketAddrV4) -> Vec<u64> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
            if *state != "0A" {
                return None;
            }
            let (ip, port) = local.split_once(':')?;
            let ip = std::net::Ipv4Addr::from(u32::from_str_radix(ip, 16).ok()?.to_ne_bytes());
            let port = u16::from_str_radix(port, 16).ok()?;
            if ip != *addr.ip() || port != addr.port() {
                return None;
            }
            inode.parse().ok()
        })
        .collect()
}

/// `1234` for the link target `socket:[1234]`.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
fn socket_inode(target: &std::path::Path) -> Option<u64> {
    target
        .to_str()?
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::path::Path;

    use super::*;

    #[test]
    fn reads_listeners_from_a_tcp_table() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 3600007F:0035 00000000:0000 0A 00000000:00000000 00:00000000 00000000   991        0 102660 1 0000000000000000 100 0 0 10 5
   1: 563412AE:A0F2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 77001 1 0000000000000000 100 0 0 10 0
   2: 563412AE:A0F2 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 77002 1 0000000000000000 20 4 30 10 -1
";
        let ip = Ipv4Addr::from(u32::from_str_radix("563412AE", 16).unwrap().to_ne_bytes());
        let addr = SocketAddrV4::new(ip, 0xA0F2);
        assert_eq!(listening_inodes(table, addr), [77001], "LISTEN only");
        let resolver = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 54), 53);
        if cfg!(target_endian = "little") {
            assert_eq!(listening_inodes(table, resolver), [102660]);
        }
        assert!(listening_inodes(table, SocketAddrV4::new(ip, 1)).is_empty());
        assert!(listening_inodes("", addr).is_empty());
        assert_eq!(socket_inode(Path::new("socket:[77001]")), Some(77001));
        assert_eq!(socket_inode(Path::new("/dev/null")), None);
    }

    /// A listener of this process is found; another port is not.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn finds_its_own_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            panic!("IPv4");
        };
        let me = std::process::id();
        assert!(listens_on(me, addr).unwrap());
        drop(listener);
        assert!(!listens_on(me, addr).unwrap());
    }
}
