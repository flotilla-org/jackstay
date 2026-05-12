use std::{
    mem::{self, size_of},
    os::{
        fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
};

use crate::error::{CaptureTransferError, Result};

const SCM_RIGHTS_FD_LEN: usize = size_of::<RawFd>();
const CONTROL_SPACE_LEN: usize = unsafe { libc::CMSG_SPACE(SCM_RIGHTS_FD_LEN as _) as usize };

#[repr(C)]
union ControlMessage {
    buf: [u8; CONTROL_SPACE_LEN],
    _align: libc::cmsghdr,
}

pub fn send_fd(stream: &UnixStream, fd: RawFd) -> Result<()> {
    let byte = [0_u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr().cast_mut().cast(),
        iov_len: byte.len(),
    };
    let mut control = ControlMessage {
        buf: [0; CONTROL_SPACE_LEN],
    };

    // SAFETY: zeroed msghdr is immediately initialized before sendmsg.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = (&raw mut control).cast();
    message.msg_controllen = CONTROL_SPACE_LEN as _;

    // SAFETY: message has enough control space for one RawFd.
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
        (*cmsg).cmsg_len = cmsg_len(SCM_RIGHTS_FD_LEN);
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        std::ptr::copy_nonoverlapping(&fd, data, 1);
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
    let mut byte = [0_u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control = ControlMessage {
        buf: [0; CONTROL_SPACE_LEN],
    };

    // SAFETY: zeroed msghdr is immediately initialized before recvmsg.
    let mut message: libc::msghdr = unsafe { mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = (&raw mut control).cast();
    message.msg_controllen = CONTROL_SPACE_LEN as _;

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
        if (*cmsg).cmsg_len != cmsg_len(SCM_RIGHTS_FD_LEN) {
            return Err(CaptureTransferError::MissingPassedFd);
        }
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        let mut fd = -1;
        std::ptr::copy_nonoverlapping(data, &mut fd, 1);
        if fd < 0 {
            return Err(CaptureTransferError::MissingPassedFd);
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn stream_fd(stream: &UnixStream) -> RawFd {
    use std::os::fd::AsFd;
    let borrowed: BorrowedFd<'_> = stream.as_fd();
    borrowed.as_raw_fd()
}

fn cmsg_len(data_len: usize) -> libc::socklen_t {
    // SAFETY: CMSG_LEN performs platform-specific header+payload sizing.
    unsafe { libc::CMSG_LEN(data_len as _) as _ }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
    };

    use crate::fdpass::{recv_fd, send_fd};

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

    fn tempfile_file_with_contents(contents: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(contents).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }
}
