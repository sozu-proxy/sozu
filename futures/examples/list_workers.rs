#[macro_use]
extern crate log;
extern crate sozu_command_futures as command;
extern crate sozu_command_lib as sozu_command;

use command::SozuCommandClient;
use sozu_command::command::{CommandRequest, CommandRequestData};
use tokio::net::UnixStream;

#[tokio::main]
pub async fn main() -> Result<(), std::io::Error> {
    env_logger::init();

    let conn = UnixStream::connect("/path/to/sozu.sock").await?;
    let mut client = SozuCommandClient::new(conn);

    let response = client
        .send(CommandRequest::new(
            String::from("message-id-42"),
            CommandRequestData::ListWorkers,
            None,
        ))
        .await;

    info!("got an answer: {:?}", response);

    Ok(())
}
