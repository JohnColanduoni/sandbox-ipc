#![feature(collections_range)]
#![feature(const_size_of)]

#[macro_use] extern crate log;
extern crate uuid;
extern crate rand;

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

mod channel;
pub mod io;
pub mod sync;
pub mod shm;

mod ser;

pub use channel::*;

pub mod os {
    #[cfg(target_os = "windows")]
    pub mod windows {
        pub use platform::*;
    }
}

#[cfg(target_os = "windows")]
#[path = "windows/mod.rs"]
mod platform;

#[cfg(unix)]
#[path = "unix/mod.rs"]
mod platform;

#[inline]
fn align(x: usize, y: usize) -> usize {
    if x > 0 && y > 0 {
        (x + (y - 1)) & !(y - 1)
    } else {
        0
    }
}

#[cfg(test)]
fn check_send(_t: &Send) {
}

const CACHE_LINE: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    use std::{thread};

    #[test]
    fn raw_message_channel_pair() {
        let reactor = tokio::reactor::Core::new().unwrap();
        let (_a, _b) = RawMessageChannel::pair(&reactor.handle()).unwrap();
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
