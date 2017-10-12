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
use sozu_command::config::Config;
use command::SozuCommandClient;
use sozu_command::data::{ConfigCommand,ConfigMessage};

fn main() {
    env_logger::init().unwrap();
    let mut core   = Core::new().unwrap();
    let handle     = core.handle();

    let stream = UnixStream::connect("/Users/geal/dev/rust/projects/yxorp/bin/sock",  &handle).unwrap();
    let mut client = SozuCommandClient::new(stream);

    core.run(
        Box::new(futures::future::lazy( || {
            client.send(ConfigMessage::new(
                String::from("message-id-42"),
                ConfigCommand::ListWorkers,
                None,
            )).and_then(|answer| {
                info!("received answer: {:?}", answer);
                Ok(())
            })
        }))

    ).unwrap()
}

