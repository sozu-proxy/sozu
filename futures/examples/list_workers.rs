#[macro_use] extern crate log;
extern crate futures;
extern crate tokio_uds;
extern crate tokio_core;
extern crate env_logger;
extern crate sozu_command_lib as sozu_command;
extern crate sozu_command_futures as command;

use futures::future::Future;
use tokio_uds::UnixStream;
use tokio_core::reactor::Core;
use command::SozuCommandClient;
use sozu_command::command::{CommandRequestData,CommandRequest};

fn main() {
    env_logger::init();
    let mut core   = Core::new().unwrap();

    core.run(
        Box::new(futures::future::lazy( || {
            UnixStream::connect("/Users/geal/dev/rust/projects/yxorp/bin/sock").and_then(|stream| {
                let mut client = SozuCommandClient::new(stream);
                client.send(CommandRequest::new(
                    String::from("message-id-42"),
                    CommandRequestData::ListWorkers,
                    None,
                )).and_then(|answer| {
                    info!("received answer: {:?}", answer);
                    Ok(())
                })
            })
        }))

    ).unwrap();
}

