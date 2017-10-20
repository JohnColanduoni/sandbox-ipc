#![feature(collections_range)]
#![feature(const_size_of)]

#[macro_use] extern crate log;
extern crate uuid;

extern crate serde;
#[macro_use] extern crate serde_derive;
extern crate bincode;

extern crate tokio_core;
#[macro_use] extern crate tokio_io;

mod tokio {
    pub use tokio_core::*;
    pub use tokio_io::*;
}
extern crate futures;

#[cfg(target_os = "windows")]
#[macro_use] extern crate winhandle;

mod io;
mod sync;
mod shm;
mod shm_queue;

mod ser;

pub use io::*;
pub use sync::*;
pub use shm::*;
pub use shm_queue::*;

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
pub mod platform;

#[cfg(unix)]
#[path = "unix/mod.rs"]
pub mod platform;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SharedMemAccess {
    Read,
    ReadWrite,
}

#[inline]
fn align(x: usize, y: usize) -> usize {
    if x > 0 && y > 0 {
        (x + (y - 1)) & !(y - 1)
    } else {
        0
    }
}

const CACHE_LINE: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    use std::{thread};

    #[test]
    fn message_channel_pair() {
        let reactor = tokio::reactor::Core::new().unwrap();
        let (_a, _b) = platform::MessageChannel::pair(&reactor.handle()).unwrap();
    }

    #[test]
    fn named_message_channel_pair() {
        let reactor = tokio::reactor::Core::new().unwrap();
        let server = platform::NamedMessageChannel::new(&reactor.handle()).unwrap();

        let name = server.name().to_os_string();
        println!("named socket: {:?}", name);
        let client_thread = thread::spawn(move || {
            let reactor = tokio::reactor::Core::new().unwrap();
            let _client = platform::NamedMessageChannel::connect(&name, None, &reactor.handle()).unwrap();
        });

        let _server = server.accept(None).unwrap();
        client_thread.join().unwrap();
    }
}
