#![feature(futures_api)]

use std::{env};
use std::time::Duration;
use std::process::{Command};

use sandbox_ipc_core::channel::{Channel, ChildChannelMetadata, ChildChannelBuilder};

use futures::Future;
use compio::local::LocalExecutor;

pub struct IpcTestContext {
    executor: LocalExecutor,
}

pub trait IpcTest {
    const RESOURCE_TX_FROM_PARENT: bool = false;
    const RESOURCE_TX_FROM_CHILD: bool = false;

    fn parent(context: &mut IpcTestContext, channel: Channel);
    fn child(context: &mut IpcTestContext, channel: Channel);
}

impl IpcTestContext {
    pub fn block_on<F, R>(&mut self, f: F) -> R where
        F: Future<Output=R>,
    {
        self.executor.block_on(f)
    }
}

#[macro_export]
macro_rules! ipc_tests {
    (
        $($test_name:ident),* $(,)*
    ) => {
        if let Ok(current_test_name) = ::std::env::var("SANDBOX_IPC_TEST_NAME") {
            $(
                if current_test_name == stringify!($test_name) {
                    $crate::run_ipc_test::<$test_name>(::std::env::var("SANDBOX_IPC_TEST_CHILD_CHANNEL").ok());
                    return;
                }
            )*
        } else {
            let mut failed = false;
            $(
                print!("test {} ... ", stringify!($test_name));
                let status = ::std::process::Command::new(::std::env::current_exe().unwrap())
                    .env("SANDBOX_IPC_TEST_NAME", stringify!($test_name))
                    .status()
                    .expect("failed to launch command");
                if !status.success() {
                    failed = true;
                    println!("err");
                } else {
                    println!("ok");
                }
            )*
            if failed {
                println!("one or more tests failed");
                ::std::process::exit(1);
            }
        }
    };
}

pub fn run_ipc_test<S: IpcTest>(child_channel_metadata: Option<String>) {
    let mut context = IpcTestContext {
        executor: LocalExecutor::new().unwrap(),
    };

    if let Some(child_channel_metadata) = child_channel_metadata {
        let channel_metadata: ChildChannelMetadata = serde_json::from_str(&child_channel_metadata).unwrap();
        let channel = channel_metadata.into_channel(&context.executor.registrar()).unwrap();
        S::child(&mut context, channel);
    } else {
        let (mut child, channel) = ChildChannelBuilder::new(Command::new(env::current_exe().unwrap()))
            .enable_resource_tx(S::RESOURCE_TX_FROM_PARENT)
            .enable_resource_rx(S::RESOURCE_TX_FROM_CHILD)
            .set_timeout(Some(Duration::from_secs(1)))
            .spawn(
                &context.executor.registrar(),
                |command, metadata| {
                    command.env("SANDBOX_IPC_TEST_CHILD_CHANNEL", serde_json::to_string(metadata).unwrap());
                    Ok(())
                },
            ).unwrap();
        S::parent(&mut context, channel);
        let status = child.wait().unwrap();
        if !status.success() {
            ::std::process::exit(1);
        }
    }
}