//! `/api/v1/host`: the daemon's identity, the Docker preflight and the
//! host's capacity.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use ship_core::{API_VERSION, HostCapacity, HostInfo};
use ship_docker::Runner;

/// Builds [`HostInfo`] now: runs the preflight through `docker` and reads
/// capacity, with free disk measured on `state_dir`'s filesystem.
pub async fn host_info(docker: &Runner, state_dir: &Path) -> HostInfo {
    let docker = ship_docker::preflight(docker).await;
    HostInfo {
        api_version: API_VERSION.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        docker,
        capacity: capacity(state_dir),
    }
}

/// CPUs, memory and free disk; whatever cannot be read is `None`.
pub fn capacity(state_dir: &Path) -> HostCapacity {
    HostCapacity {
        cpus: std::thread::available_parallelism()
            .ok()
            .and_then(|n| u64::try_from(n.get()).ok()),
        memory_bytes: memory_bytes(),
        disk_free_bytes: disk_free_bytes(state_dir),
    }
}

/// `MemTotal` from `/proc/meminfo`. The daemon runs on Linux only
/// (ADR-0002), so elsewhere this is `None`.
fn memory_bytes() -> Option<u64> {
    if cfg!(target_os = "linux") {
        parse_mem_total(&std::fs::read_to_string("/proc/meminfo").ok()?)
    } else {
        None
    }
}

/// The `MemTotal:  N kB` line of a `/proc/meminfo` text, in bytes.
fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let rest = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let mut words = rest.split_whitespace();
    let n: u64 = words.next()?.parse().ok()?;
    match words.next() {
        Some("kB") => n.checked_mul(1024),
        _ => None,
    }
}

/// Bytes available to unprivileged users on the filesystem holding `path`.
pub(crate) fn disk_free_bytes(path: &Path) -> Option<u64> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero `statvfs` is a valid value of this plain C struct.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call and
    // `stat` is a valid, writable `statvfs`.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    #[allow(clippy::useless_conversion)] // field widths differ by platform
    let (avail, frsize) = (u64::from(stat.f_bavail), u64::from(stat.f_frsize));
    avail.checked_mul(frsize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_total_parses() {
        let text = "MemTotal:       16314660 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_mem_total(text), Some(16_314_660 * 1024));
        assert_eq!(parse_mem_total("MemFree: 1 kB\n"), None);
        assert_eq!(parse_mem_total("MemTotal: x kB\n"), None);
        assert_eq!(parse_mem_total("MemTotal: 12 MB\n"), None);
    }

    #[test]
    fn capacity_reads_this_host() {
        let dir = tempfile::tempdir().unwrap();
        let c = capacity(dir.path());
        assert!(c.cpus.is_some_and(|n| n >= 1), "{c:?}");
        assert!(c.disk_free_bytes.is_some_and(|n| n > 0), "{c:?}");
        if cfg!(target_os = "linux") {
            assert!(c.memory_bytes.is_some_and(|n| n > 0), "{c:?}");
        }
    }

    #[test]
    fn missing_state_dir_has_no_disk_figure() {
        let c = capacity(Path::new("/nonexistent/ship-test/state"));
        assert_eq!(c.disk_free_bytes, None);
    }
}
