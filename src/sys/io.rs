//! Low-level stdio inspection used for orphan detection, plus file-descriptor
//! helpers (non-blocking mode, EINTR/EAGAIN-safe writes).

#[cfg(unix)]
use std::os::fd::AsFd;

/// Whether this process's stdin appears broken/invalid (a heuristic for
/// detecting that the launching parent is gone).
///
/// Unix: a non-blocking `poll` of fd 0 that reports `POLLERR`/`POLLNVAL` (but
/// *not* `POLLHUP`, which is the normal end of a piped payload). Windows: always
/// false — pipe close semantics differ and orphan detection there relies on
/// other signals.
pub fn stdin_appears_broken() -> bool {
    #[cfg(unix)]
    {
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, nfds=1, timeout=0 (non-blocking).
        let ret = unsafe { libc::poll(&mut pfd as *mut _, 1, 0) };
        if ret < 0 {
            return true;
        }
        (pfd.revents & (libc::POLLERR | libc::POLLNVAL)) != 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Set a file descriptor to non-blocking mode.
///
/// Unix: uses `fcntl(F_SETFL)` to add `O_NONBLOCK`. The fd must be a valid
/// open descriptor (e.g. a PTY master, pipe, or socket).
#[cfg(unix)]
pub fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> nix::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = fcntl(fd.as_fd(), FcntlArg::F_GETFL)?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

/// Write all bytes to a file descriptor, retrying on `EINTR` and `EAGAIN`.
///
/// Unlike `std::io::Write::write_all`, this uses `nix::unistd::write` (raw
/// `write(2)`) and handles `EINTR` by retrying immediately and `EAGAIN` by
/// sleeping 1 ms before retrying.
#[cfg(unix)]
pub fn write_all<F: AsFd>(fd: &F, data: &[u8]) -> nix::Result<()> {
    use nix::errno::Errno;
    use nix::unistd::write;
    let mut written = 0;
    while written < data.len() {
        match write(fd, &data[written..]) {
            Ok(n) => written += n,
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
