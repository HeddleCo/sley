//! Process ancestry helpers for Git-compatible trace2 output.
//!
//! This crate intentionally exposes a tiny safe API and keeps platform
//! inspection details local. Sley only calls it when trace2 targets are active,
//! so normal command execution pays no process-walk cost.

use std::sync::OnceLock;

static PROCESS_ANCESTRY: OnceLock<Vec<String>> = OnceLock::new();

/// Return the current process ancestry, starting with the immediate parent.
///
/// Names are cached after the first call because trace2 may need to emit both
/// the current process row and a synthetic child row during in-process alias
/// expansion.
#[must_use]
pub fn process_ancestry() -> &'static [String] {
    PROCESS_ANCESTRY.get_or_init(platform::process_ancestry)
}

#[cfg(unix)]
pub fn duplicate_fd(fd: i32) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;

    // SAFETY: `dup` only inspects the numeric file descriptor and returns either
    // a fresh descriptor owned by this process or -1 with errno set.
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `duplicated` is a newly returned descriptor, so wrapping it in
    // `File` gives Rust ownership and closes exactly that duplicate on drop.
    Ok(unsafe { std::fs::File::from_raw_fd(duplicated) })
}

#[cfg(target_os = "macos")]
mod platform {
    use std::mem::{MaybeUninit, size_of};

    const NR_PIDS_LIMIT: usize = 10;
    const MAXCOMLEN: usize = 16;
    const PROC_PIDTBSDINFO: libc::c_int = 3;

    struct ProcInfo {
        name: String,
        ppid: libc::pid_t,
    }

    #[repr(C)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: libc::uid_t,
        pbi_gid: libc::gid_t,
        pbi_ruid: libc::uid_t,
        pbi_rgid: libc::gid_t,
        pbi_svuid: libc::uid_t,
        pbi_svgid: libc::gid_t,
        rfu_1: u32,
        pbi_comm: [libc::c_char; MAXCOMLEN],
        pbi_name: [libc::c_char; 2 * MAXCOMLEN],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    pub(super) fn process_ancestry() -> Vec<String> {
        // SAFETY: getppid has no preconditions and does not touch Rust-managed
        // memory.
        let mut pid = unsafe { libc::getppid() };
        let mut names = Vec::new();
        for _ in 0..NR_PIDS_LIMIT {
            if pid <= 0 {
                break;
            }
            let Some(info) = proc_info(pid) else {
                break;
            };
            if !info.name.is_empty() {
                names.push(info.name);
            }
            if info.ppid <= 0 || info.ppid == pid {
                break;
            }
            pid = info.ppid;
        }
        names
    }

    fn proc_info(pid: libc::pid_t) -> Option<ProcInfo> {
        let mut proc = MaybeUninit::<ProcBsdInfo>::uninit();
        let size = size_of::<ProcBsdInfo>();
        let buffer_size = libc::c_int::try_from(size).ok()?;

        // SAFETY: `proc` points to a writable proc_bsdinfo-compatible buffer
        // with `buffer_size` set to its exact byte length. PROC_PIDTBSDINFO is a
        // read-only query and arg is unused for this flavor.
        let rc = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                proc.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if usize::try_from(rc).ok()? < size {
            return None;
        }

        // SAFETY: proc_pidinfo reported that it wrote a full ProcBsdInfo.
        let proc = unsafe { proc.assume_init() };
        Some(ProcInfo {
            name: comm_to_string(&proc.pbi_comm),
            ppid: libc::pid_t::try_from(proc.pbi_ppid).ok()?,
        })
    }

    fn comm_to_string(comm: &[libc::c_char]) -> String {
        let bytes = comm
            .iter()
            .take_while(|ch| **ch != 0)
            .map(|ch| *ch as u8)
            .collect::<Vec<_>>();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::path::PathBuf;

    const NR_PIDS_LIMIT: usize = 10;

    pub(super) fn process_ancestry() -> Vec<String> {
        let mut names = Vec::new();
        let Some(mut pid) = parent_pid(std::process::id()) else {
            return names;
        };
        for _ in 0..NR_PIDS_LIMIT {
            if pid == 0 {
                break;
            }
            if let Some(name) = process_name(pid).filter(|name| !name.is_empty()) {
                names.push(name);
            }
            let Some(next) = parent_pid(pid) else {
                break;
            };
            if next == 0 || next == pid {
                break;
            }
            pid = next;
        }
        names
    }

    fn process_name(pid: u32) -> Option<String> {
        let mut path = proc_path(pid);
        path.push("comm");
        if let Ok(raw) = fs::read_to_string(&path) {
            return Some(raw.trim_end_matches('\n').to_string());
        }
        stat_name(pid)
    }

    fn parent_pid(pid: u32) -> Option<u32> {
        let stat = stat(pid)?;
        let close = stat.rfind(')')?;
        let rest = stat.get(close + 1..)?.trim_start();
        let mut fields = rest.split_whitespace();
        let _state = fields.next()?;
        fields.next()?.parse().ok()
    }

    fn stat_name(pid: u32) -> Option<String> {
        let stat = stat(pid)?;
        let open = stat.find('(')?;
        let close = stat.rfind(')')?;
        (close > open).then(|| stat[open + 1..close].to_string())
    }

    fn stat(pid: u32) -> Option<String> {
        let mut path = proc_path(pid);
        path.push("stat");
        fs::read_to_string(path).ok()
    }

    fn proc_path(pid: u32) -> PathBuf {
        let mut path = PathBuf::from("/proc");
        path.push(pid.to_string());
        path
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    pub(super) fn process_ancestry() -> Vec<String> {
        Vec::new()
    }
}
