use std::os::unix::prelude::*;

pub trait ResourceExt: Sized + FromRawFd {
    fn from_fd<F: IntoRawFd>(fd: F) -> Self;
    fn as_raw_fd(&self) -> Option<RawFd>;
    fn into_raw_fd(self) -> Result<RawFd, Self>;
}

pub trait ResourceRefExt<'a> {
    fn with_fd<F: AsRawFd>(fd: &'a F) -> Self;
    unsafe fn with_raw_fd(fd: RawFd) -> Self;
    fn as_raw_fd(&self) -> Option<RawFd>;
}

pub trait ResourceTransceiverExt {
    fn inline() -> Self;
    fn inline_tx_only() -> Self;
    fn inline_rx_only() -> Self;
}