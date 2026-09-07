use std::{
    mem::{self, size_of, size_of_val},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
};

use crate::error::{CaptureTransferError, Result};

pub fn send_fd(stream: &UnixStream, fd: RawFd) -> Result<()> {
    send_fds(stream, &[fd])
}

pub fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> Result<()> {
    if fds.is_empty() {
        return Err(CaptureTransferError::FdPassing {
            operation: "sendmsg",
            message: "at least one fd is required".to_string(),
        });
    }
    let byte = [0_u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr().cast_mut().cast(),
        iov_len: byte.len(),
    };
    let fd_bytes = size_of_val(fds);
    let control_len = cmsg_space(fd_bytes);
    let mut control = aligned_control_buffer(control_len);

    // SAFETY: zeroed msghdr is immediately initialized before sendmsg.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control_len as _;

    // SAFETY: message has enough aligned control space for all RawFd values.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() {
            return Err(CaptureTransferError::FdPassing {
                operation: "CMSG_FIRSTHDR",
                message: "no control header available".to_string(),
            });
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = cmsg_len(fd_bytes) as _;
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        std::ptr::copy_nonoverlapping(fds.as_ptr(), data, fds.len());
    }

    // SAFETY: stream fd is valid and message points to initialized iov/control buffers.
    let sent = unsafe { libc::sendmsg(stream_fd(stream), &message, 0) };
    if sent < 0 {
        return Err(CaptureTransferError::FdPassing {
            operation: "sendmsg",
            message: std::io::Error::last_os_error().to_string(),
        });
    }
    Ok(())
}

pub fn recv_fd(stream: &UnixStream) -> Result<OwnedFd> {
    let mut fds = recv_fds(stream, 1)?;
    if fds.len() != 1 {
        return Err(CaptureTransferError::MissingPassedFd);
    }
    Ok(fds.remove(0))
}

pub fn recv_fds(stream: &UnixStream, max_fds: usize) -> Result<Vec<OwnedFd>> {
    if max_fds == 0 {
        return Err(CaptureTransferError::FdPassing {
            operation: "recvmsg",
            message: "max_fds must be non-zero".to_string(),
        });
    }
    let mut byte = [0_u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let max_fd_bytes = max_fds * size_of::<RawFd>();
    let control_len = cmsg_space(max_fd_bytes);
    let mut control = aligned_control_buffer(control_len);

    // SAFETY: zeroed msghdr is immediately initialized before recvmsg.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control_len as _;

    // SAFETY: stream fd is valid and message points to initialized iov/control buffers.
    let received = unsafe { libc::recvmsg(stream_fd(stream), &mut message, 0) };
    if received < 0 {
        return Err(CaptureTransferError::FdPassing {
            operation: "recvmsg",
            message: std::io::Error::last_os_error().to_string(),
        });
    }

    // SAFETY: message was filled by recvmsg; cmsg traversal follows libc macros.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&message);
        if cmsg.is_null() || (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(CaptureTransferError::MissingPassedFd);
        }
        // When the control buffer is too small (MSG_CTRUNC), macOS leaves
        // cmsg_len at the sender's untruncated length; only msg_controllen
        // reflects what the kernel actually delivered. Interpreting the
        // difference as fds would read bytes the kernel never wrote — and
        // then close whatever fd numbers those bytes happen to spell.
        let control_message_len = ((*cmsg).cmsg_len as usize).min(message.msg_controllen as usize);
        if control_message_len < cmsg_len(0) {
            return Err(CaptureTransferError::MissingPassedFd);
        }
        let fd_bytes = control_message_len - cmsg_len(0);
        if fd_bytes == 0 {
            return Err(CaptureTransferError::MissingPassedFd);
        }
        let fd_count = fd_bytes / size_of::<RawFd>();
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        let raw_fds = std::slice::from_raw_parts(data, fd_count).to_vec();
        if fd_bytes % size_of::<RawFd>() != 0 {
            close_raw_fds(&raw_fds);
            return Err(CaptureTransferError::MissingPassedFd);
        }
        if message.msg_flags & libc::MSG_CTRUNC != 0 {
            close_raw_fds(&raw_fds);
            return Err(CaptureTransferError::FdPassing {
                operation: "recvmsg",
                message: "ancillary data truncated (MSG_CTRUNC)".to_string(),
            });
        }
        if fd_count > max_fds {
            close_raw_fds(&raw_fds);
            return Err(CaptureTransferError::FdPassing {
                operation: "recvmsg",
                message: format!("received {fd_count} fds, maximum is {max_fds}"),
            });
        }
        let invalid_fd = raw_fds.iter().any(|fd| *fd < 0);
        if invalid_fd {
            close_raw_fds(&raw_fds);
            return Err(CaptureTransferError::MissingPassedFd);
        }
        Ok(raw_fds.into_iter().map(|fd| OwnedFd::from_raw_fd(fd)).collect())
    }
}

fn close_raw_fds(fds: &[RawFd]) {
    for fd in fds.iter().copied().filter(|fd| *fd >= 0) {
        // SAFETY: these fds were installed into this process by recvmsg and
        // have not been wrapped in OwnedFd because the message was invalid.
        unsafe {
            libc::close(fd);
        }
    }
}

fn stream_fd(stream: &UnixStream) -> RawFd {
    use std::os::fd::AsFd;
    let borrowed: BorrowedFd<'_> = stream.as_fd();
    borrowed.as_raw_fd()
}

fn cmsg_len(data_len: usize) -> usize {
    // SAFETY: CMSG_LEN performs platform-specific header+payload sizing.
    unsafe { libc::CMSG_LEN(data_len as _) as usize }
}

fn cmsg_space(data_len: usize) -> usize {
    // SAFETY: CMSG_SPACE performs platform-specific aligned control sizing.
    unsafe { libc::CMSG_SPACE(data_len as _) as usize }
}

fn aligned_control_buffer(len: usize) -> Vec<usize> {
    let words = len.div_ceil(size_of::<usize>());
    vec![0; words]
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use crate::fdpass::{recv_fd, recv_fds, send_fd, send_fds};

    #[test]
    fn sends_file_descriptor_over_unix_stream() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let mut file = tempfile_file_with_contents(b"frame-bytes");

        send_fd(&sender, file.as_raw_fd()).unwrap();

        let mut received = File::from(recv_fd(&receiver).unwrap());
        let mut contents = Vec::new();
        received.read_to_end(&mut contents).unwrap();

        assert_eq!(contents, b"frame-bytes");

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut original = Vec::new();
        file.read_to_end(&mut original).unwrap();
        assert_eq!(original, b"frame-bytes");
    }

    #[test]
    fn sends_multiple_file_descriptors_over_unix_stream() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let first = tempfile_file_with_contents(b"first");
        let second = tempfile_file_with_contents(b"second");

        send_fds(&sender, &[first.as_raw_fd(), second.as_raw_fd()]).unwrap();

        let mut received = recv_fds(&receiver, 2).unwrap();
        assert_eq!(received.len(), 2);
        let mut first_received = File::from(received.remove(0));
        let mut second_received = File::from(received.remove(0));

        let mut first_contents = Vec::new();
        let mut second_contents = Vec::new();
        first_received.read_to_end(&mut first_contents).unwrap();
        second_received.read_to_end(&mut second_contents).unwrap();

        assert_eq!(first_contents, b"first");
        assert_eq!(second_contents, b"second");
    }

    #[test]
    fn rejects_truncated_file_descriptor_messages() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let first = tempfile_file_with_contents(b"first");
        let second = tempfile_file_with_contents(b"second");

        send_fds(&sender, &[first.as_raw_fd(), second.as_raw_fd()]).unwrap();

        // On macOS a truncated cmsg keeps the sender's untruncated cmsg_len,
        // so the undelivered tail of the zeroed control buffer spells fd 0.
        // Without the msg_controllen clamp the truncation cleanup closed it
        // (stdin) out from under the whole process.
        let fd0_open_before = unsafe { libc::fcntl(0, libc::F_GETFD) } != -1;
        assert!(recv_fds(&receiver, 1).is_err());
        let fd0_open_after = unsafe { libc::fcntl(0, libc::F_GETFD) } != -1;
        assert_eq!(fd0_open_after, fd0_open_before, "truncated recv closed an fd it never received");
    }

    fn tempfile_file_with_contents(contents: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(contents).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }
}
