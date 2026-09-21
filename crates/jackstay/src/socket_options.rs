//! Socket-local protection for libraries embedded in hosts with default SIGPIPE.
use std::{io, os::unix::net::UnixStream};

pub(crate) fn suppress_sigpipe(stream: &UnixStream) -> io::Result<()> {
    // On Darwin, a failed send can signal another thread in the process even
    // when the writer blocks SIGPIPE. Imported FDs and UnixStream::pair do not
    // necessarily have the option Rust sets on freshly connected sockets.
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let enabled: libc::c_int = 1;
        // SAFETY: the borrowed socket is live and the option buffer is valid.
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    let _ = stream;
    Ok(())
}
