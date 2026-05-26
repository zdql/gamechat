//! Where gamechat puts its control sockets.
//!
//! macOS doesn't set `XDG_RUNTIME_DIR`, so we fall back to `$TMPDIR`. The
//! returned directory always contains the username so multiple users on the
//! same machine never collide. Socket paths look like
//! `<dir>/gamechat-<user>/<pid>.sock`.

use std::path::PathBuf;

pub(crate) fn runtime_dir() -> Result<PathBuf, String> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("TMPDIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "default".to_string());
    Ok(base.join(format!("gamechat-{user}")))
}

pub(crate) fn socket_path_for_pid(pid: u32) -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join(format!("{pid}.sock")))
}

/// Scan the runtime dir for `*.sock` files. Stale entries (no matching live
/// process) are filtered out.
pub(crate) fn discover_sockets() -> Result<Vec<PathBuf>, String> {
    let dir = runtime_dir()?;
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("failed to read runtime dir {}: {err}", dir.display())),
    };
    let mut sockets: Vec<PathBuf> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("sock") {
            continue;
        }
        // Drop sockets whose pid no longer exists; the server unlinks on
        // graceful shutdown, but crashes leave files behind.
        if let Some(pid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        {
            if !pid_is_alive(pid) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        }
        sockets.push(path);
    }
    sockets.sort();
    Ok(sockets)
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // Sending signal 0 is a no-op probe that returns success iff the process
    // exists and we have permission to signal it.
    unsafe { libc_kill(pid as i32, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

// libc is in std on unix via libc crate; we don't want to pull libc as a
// dep, so duplicate the EPERM constant here.
#[cfg(unix)]
#[allow(non_snake_case)]
mod libc {
    pub const EPERM: i32 = 1;
}
