#![feature(async_await, await_macro)]

#[macro_use] extern crate sandbox_ipc_test;

use std::io::{Read, Write, Seek, SeekFrom};
use std::fs::File;

use sandbox_ipc_core::{
    channel::{Channel},
    resource::{Resource},
};
use sandbox_ipc_test::{IpcRawTest, IpcTestContext};

fn main() {
    ipc_tests! {
        SendFile,
    }
}

pub struct SendFile;

const MESSAGE: &[u8] = b"Hello World!";

#[cfg(unix)]
impl IpcRawTest for SendFile {
    const RESOURCE_TX_FROM_PARENT: bool = true;

    fn parent_raw(context: &mut IpcTestContext, mut channel: Channel) {
        use sandbox_ipc_core::os::unix::*;

        context.block_on(async {
            let mut f = tempfile::tempfile().unwrap();
            f.write_all(MESSAGE).unwrap();
            let mut sender = channel.send_with_resources().unwrap();
            let buffer = serde_json::to_vec(&sender.move_resource(Resource::from_fd(f)).unwrap()).unwrap();
            await!(sender.finish(&buffer)).unwrap();
        });
    }

    fn child_raw(context: &mut IpcTestContext, mut channel: Channel) {
        use std::os::unix::prelude::*;
        use sandbox_ipc_core::os::unix::*;

        context.block_on(async {
            let mut buffer = vec![0u8; 1024];
            let mut receiver = await!(channel.recv_with_resources(1, &mut buffer)).unwrap();
            let resource = receiver.recv_resource(serde_json::from_slice(&buffer[..receiver.data_len()]).unwrap()).unwrap();
            let mut f = unsafe { File::from_raw_fd(resource.into_raw_fd().unwrap()) };
            f.seek(SeekFrom::Start(0)).unwrap();
            let mut file_buffer = Vec::new();
            f.read_to_end(&mut file_buffer).unwrap();
            assert_eq!(MESSAGE, &*file_buffer);
        });
    }
}
