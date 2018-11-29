pub mod resource;
pub mod channel;

#[cfg_attr(unix, path = "platform/unix.rs")]
mod platform;

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use crate::platform::{SendableFd, SendableFdRef};
    }
}