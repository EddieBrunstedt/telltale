//! Watching a directory with inotify, for gauges that react to changes in
//! one instead of measuring on an interval.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A non-blocking inotify instance watching `dir` for `mask`. IN_ONLYDIR is
/// always added, so a file where the directory should be is an error.
pub(crate) fn watch(dir: &Path, mask: u32) -> io::Result<OwnedFd> {
    // SAFETY: inotify_init1 takes only flags; a non-negative result is a new
    // fd that nothing else owns.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "contains a NUL"))?;
    // SAFETY: fd is an inotify instance and path a valid C string.
    if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask | libc::IN_ONLYDIR) }
        < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Reads every pending event. Which entry changed does not matter, since
/// every change means reading the directory again; what matters is whether
/// the directory itself is still there, which this returns.
pub(crate) fn drain(inotify: &OwnedFd) -> bool {
    const GONE: u32 = libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED;
    let mut buf = [0u8; 4096];
    let mut there = true;
    loop {
        // SAFETY: buf is valid for writes of its full length.
        let n = unsafe { libc::read(inotify.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            // EAGAIN: drained. Anything else: the next poll will say.
            return there;
        }
        let mut at = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while at + header <= n as usize {
            // SAFETY: the kernel writes whole events; read_unaligned because
            // a u8 buffer promises no alignment.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            there &= event.mask & GONE == 0;
            at += header + event.len as usize;
        }
    }
}
