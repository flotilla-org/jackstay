//! Shared bounded length-prefixed JSON framing for private typed channels.
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
};

use crate::local::Stream;
const MAX_FRAME: usize = 128 * 1024;
const MAX_BUFFER: usize = 512 * 1024;
pub(crate) struct Framed {
    stream: Stream,
    input: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    pub(crate) queued: usize,
}
impl Framed {
    pub(crate) fn new(stream: Stream) -> io::Result<Self> {
        #[cfg(unix)]
        crate::socket_options::suppress_sigpipe(&stream)?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            input: Vec::new(),
            output: VecDeque::new(),
            offset: 0,
            queued: 0,
        })
    }
    pub(crate) fn send<T: serde::Serialize>(&mut self, msg: T) -> io::Result<()> {
        let json = serde_json::to_vec(&msg)?;
        if json.is_empty() || json.len() > MAX_FRAME || self.queued + json.len() + 4 > MAX_BUFFER {
            return Err(io::Error::other("input wire bound exceeded"));
        }
        let mut frame = Vec::with_capacity(json.len() + 4);
        frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
        frame.extend(json);
        self.queued += frame.len();
        self.output.push_back(frame);
        Ok(())
    }
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        while let Some(frame) = self.output.front() {
            match self.stream.write(&frame[self.offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.offset += n;
                    self.queued -= n;
                    if self.offset == frame.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    /// Everything queued has been handed to the transport. A Windows pipe can
    /// still be completing an accepted write in the background.
    pub(crate) fn idle(&mut self) -> io::Result<bool> {
        if !self.output.is_empty() {
            return Ok(false);
        }
        match self.stream.flush() {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub(crate) fn receive<T: serde::de::DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
        loop {
            if self.input.len() >= 4 {
                let size = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
                if size == 0 || size > MAX_FRAME {
                    return Err(io::Error::other("invalid input frame size"));
                }
                if self.input.len() >= size + 4 {
                    let msg = serde_json::from_slice(&self.input[4..size + 4])?;
                    self.input.drain(..size + 4);
                    return Ok(Some(msg));
                }
            }
            let mut buf = [0; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.input.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}
