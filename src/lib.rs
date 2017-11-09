//! Convenient and powerful cross-platform IPC primitives, designed with privilege separation in mind.
//!
//! The core of this crate is the `MessageChannel`. It not only automatically serializes and deserializes messages via
//! `serde`, but can even send OS resources like files to other processes. To get started, spawn a child process with
//! `MessageChannel::establish_with_child` and transmit the initial channel with an environment variable:
//!  
//! ```rust,no_run
//! extern crate futures;
//! extern crate tokio_core;
//! extern crate sandbox_ipc as ipc;
//! extern crate serde_json as json;
//! 
//! use std::{fs, env};
//! use std::process::Command;
//! use std::io::Write;
//!
//! use ipc::io::SendableFile;
//! use futures::prelude::*; 
//! use tokio_core::reactor::Core;
//! 
//! const CHANNEL_ENV_VAR: &str = "ENV_IPC_CHANNEL";
//! 
//! fn main() {
//!     // IO operations are done within a Tokio event loop
//!     let mut core = Core::new().unwrap();
//!     let handle = core.handle();
//!     
//!     let mut child_command = Command::new("some_child_executable");
//!     let (channel, child) = ipc::MessageChannel::<SendableFile, i32>::establish_with_child(
//!         &mut child_command, 8192, &handle, |command, child_channel| {
//!             command
//!                 .env(CHANNEL_ENV_VAR, json::to_string(child_channel).unwrap())
//!                 .spawn()
//!         }
//!     ).unwrap();
//! 
//!     let secret_file = fs::File::create("secret_file.txt").unwrap();
//!     let channel = core.run(channel.send(SendableFile(secret_file))).unwrap();
//! 
//!     let (reason, _channel) = core.run(channel.into_future()).map_err(|(err, _)| err).unwrap();
//!     let reason = reason.unwrap();
//!     assert_eq!(42i32, reason);
//! }
//! 
//! fn child_main() {
//!     let mut core = Core::new().unwrap();
//!     let handle = core.handle();
//! 
//!     let channel: ipc::ChildMessageChannel = 
//!         json::from_str(&env::var(CHANNEL_ENV_VAR).unwrap()).unwrap();
//!     let channel = channel.into_channel::<i32, SendableFile>(&handle).unwrap();
//! 
//!     let (secret_file, channel) = core.run(channel.into_future())
//!         .map_err(|(err, _)| err).unwrap();
//!     let SendableFile(mut secret_file) = secret_file.unwrap();
//! 
//!     write!(&mut secret_file, "psst").unwrap();
//! 
//!     let _channel = core.run(channel.send(42i32)).unwrap();
//! }
//! ```
//! 

#![feature(collections_range)]
#![feature(const_size_of, const_fn)]

#[macro_use] extern crate log;
extern crate uuid;
extern crate rand;
#[macro_use] extern crate lazy_static;

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
