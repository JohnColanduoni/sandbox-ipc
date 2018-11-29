#![feature(async_await, await_macro)]

#[macro_use] extern crate sandbox_ipc_test;

use std::io::{Read, Write, Seek, SeekFrom};
use std::fs::File;

use sandbox_ipc_serde::{
    channel::{Channel},
    resource::{SendableFile},
};
use sandbox_ipc_test::{IpcTest, IpcTestContext};

fn main() {
    ipc_tests! {
        SendFile,
    }
}

pub struct SendFile;

const MESSAGE: &[u8] = b"Hello World!";

impl IpcTest for SendFile {
    const RESOURCE_TX_FROM_PARENT: bool = true;

    fn parent(context: &mut IpcTestContext, mut channel: Channel) {
        context.block_on(async {
            let mut f = tempfile::tempfile().unwrap();
            f.write_all(MESSAGE).unwrap();
            let msg = SendableFile(&f);
            await!(channel.send(&msg)).unwrap()
        });
    }

    fn child(context: &mut IpcTestContext, mut channel: Channel) {
        context.block_on(async {
            let SendableFile(mut f) = await!(channel.recv()).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            let mut file_buffer = Vec::new();
            f.read_to_end(&mut file_buffer).unwrap();
            assert_eq!(MESSAGE, &*file_buffer);
        });
    }
}
