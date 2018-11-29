#[macro_use] extern crate sandbox_ipc_test;

use sandbox_ipc_core::channel::{Channel};
use sandbox_ipc_test::{IpcTest, IpcTestContext};

fn main() {
    ipc_tests! {
        SendFile,
    }
}

pub struct SendFile;

impl IpcTest for SendFile {
    type T = ();
    type R = ();

    fn parent(context: &mut IpcTestContext, channel: Channel<Self::T, Self::R>) {
    }

    fn child(context: &mut IpcTestContext, channel: Channel<Self::R, Self::T>) {
        unimplemented!()
    }
}