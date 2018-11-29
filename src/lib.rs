
pub mod resource {
    pub use sandbox_ipc_core::resource::*;
    pub use sandbox_ipc_serde::resource::*;
}

pub mod channel {
    pub use sandbox_ipc_serde::channel::*;
    pub use sandbox_ipc_core::channel as raw;
}

pub mod os {
    #[cfg(unix)]
    pub mod unix {
        pub use sandbox_ipc_core::os::unix::*;
        pub use sandbox_ipc_serde::os::unix::*;
    }

    #[cfg(target_os = "macos")]
    pub mod macos {
        pub use sandbox_ipc_core::os::macos::*;
    }
}