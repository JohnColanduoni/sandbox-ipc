#![feature(futures_api, async_await, await_macro, pin, arbitrary_self_types)]

#[macro_use] extern crate log;

#[macro_use] mod macros;
pub mod channel;
pub mod resource;

#[cfg_attr(target_os = "macos", path = "platform/macos.rs")]
mod platform;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use crate::platform::unix::{ResourceExt, ResourceRefExt, ResourceTransceiverExt};
    }

    #[cfg(target_os = "macos")]
    pub mod macos {
        pub use crate::platform::{ResourceExt, ResourceRefExt, ResourceTransceiverExt};
    }
}
