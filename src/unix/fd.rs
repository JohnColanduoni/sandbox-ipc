use platform::SendableFd;

use std::io;
use std::os::unix::prelude::*;

use serde::{Serialize, Deserialize, Serializer, Deserializer};
use platform::libc;
use platform::mio::{Evented, Poll, Token, Ready, PollOpt};
use platform::mio::unix::EventedFd;

#[derive(Debug)]
pub struct ScopedFd(pub libc::c_int);

impl Drop for ScopedFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0); }
    }
}

impl FromRawFd for ScopedFd {
    unsafe fn from_raw_fd(fd: libc::c_int) -> Self {
        ScopedFd(fd)
    }
}

impl AsRawFd for ScopedFd {
    fn as_raw_fd(&self) -> libc::c_int {
        self.0
    }
}

impl Serialize for ScopedFd {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where S: Serializer
    {
        SendableFd(self.0).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ScopedFd {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where D: Deserializer<'de>
    {
        Ok(ScopedFd(SendableFd::deserialize(deserializer)?.0))
    }
}

impl Evented for ScopedFd {
    fn register(&self,
                poll: &Poll,
                token: Token,
                events: Ready,
                opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.0).register(poll, token, events, opts)
    }

    fn reregister(&self,
                  poll: &Poll,
                  token: Token,
                  events: Ready,
                  opts: PollOpt) -> io::Result<()> {
        EventedFd(&self.0).reregister(poll, token, events, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        EventedFd(&self.0).deregister(poll)
    }
}