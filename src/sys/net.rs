//! Socket readiness waiting, used by the notify servers to block until a
//! wake-up connection arrives instead of busy-polling.

use std::net::TcpListener;
use std::time::Duration;

/// Send a JSON request to a Unix domain socket and read the response line.
///
/// Returns `Ok(())` on success. The response body is discarded — this is a
/// fire-and-forget label push, not a request/reply.
#[cfg(unix)]
pub fn send_unix_request(socket_path: &str, request: &serde_json::Value) -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let socket_timeout = std::time::Duration::from_millis(200);
    let mut stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect: {socket_path}: {e}"))?;
    let _ = stream.set_read_timeout(Some(socket_timeout));
    let _ = stream.set_write_timeout(Some(socket_timeout));
    writeln!(stream, "{request}").map_err(|e| format!("write: {e}"))?;
    let mut response = String::new();
    BufReader::new(&stream)
        .read_line(&mut response)
        .map_err(|e| format!("read: {e}"))?;
    Ok(())
}

/// Stub for non-Unix platforms.
#[cfg(not(unix))]
pub fn send_unix_request(_socket_path: &str, _request: &serde_json::Value) -> Result<(), String> {
    Err("unix sockets unavailable on this platform".into())
}

/// Block until `listener` has an incoming connection ready to accept, or
/// `timeout` elapses. Returns true if readable, false on timeout.
///
/// Unix: `poll(POLLIN)`. Windows: `select` over the socket.
pub fn wait_readable(listener: &TcpListener, timeout: Duration) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, nfds=1, bounded timeout.
        let ret = unsafe { libc::poll(&mut pfd as *mut _, 1, timeout_ms) };
        ret > 0
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{FD_SET, SOCKET, TIMEVAL, select};

        let mut set: FD_SET = unsafe { std::mem::zeroed() };
        set.fd_count = 1;
        set.fd_array[0] = listener.as_raw_socket() as SOCKET;
        let tv = TIMEVAL {
            tv_sec: timeout.as_secs().min(i32::MAX as u64) as i32,
            tv_usec: timeout.subsec_micros() as i32,
        };
        // SAFETY: read set holds one valid socket; nfds is ignored on Windows.
        let ret = unsafe { select(0, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), &tv) };
        ret > 0
    }
}
