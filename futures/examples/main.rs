#[macro_use]
extern crate log;
extern crate env_logger;
extern crate futures;
extern crate sozu_command_futures as command;
extern crate sozu_command_lib as sozu_command;
extern crate tokio_core;
extern crate tokio_uds;

use command::SozuCommandClient;
use futures::future::Future;
use sozu_command::command::{CommandRequest, CommandRequestData};
use tokio_core::reactor::Core;
use tokio_uds::UnixStream;

fn main() {
    env_logger::init();
    let mut core = Core::new().unwrap();

    core.run(Box::new(futures::future::lazy(|| {
        UnixStream::connect("/Users/geal/dev/rust/projects/yxorp/bin/sock").and_then(|stream| {
            let mut client = SozuCommandClient::new(stream);
            client
                .send(CommandRequest::new(
                    String::from("message-id-42"),
                    CommandRequestData::ListWorkers,
                    None,
                ))
                .and_then(|answer| {
                    info!("received answer: {:?}", answer);
                    Ok(())
                })
        })
    })))
    .unwrap();
}
