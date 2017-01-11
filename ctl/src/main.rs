#![feature(proc_macro)]
#[macro_use] extern crate clap;
#[macro_use] extern crate serde_derive;
extern crate mio;
extern crate mio_uds;
extern crate nix;
extern crate libc;
extern crate time;
extern crate toml;
extern crate serde;
extern crate serde_json;
extern crate sozu_lib as sozu;

mod config;

use mio_uds::UnixStream;
use std::net::{UdpSocket,ToSocketAddrs};
use std::collections::HashMap;
use clap::{App,Arg,SubCommand};
use sozu::messages::Command;
use sozu::command::CommandChannel;

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
 pub enum ConfigMessageStatus {
   Ok,
   Processing,
   Error
 }

#[derive(Debug,Clone,PartialEq,Eq,Hash, Serialize)]
pub enum ConfigCommand {
  ProxyConfiguration(Command),
  SaveState(String),
  LoadState(String),
  DumpState,
}

//FIXME: maybe need a custom serialize here
#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize)]
pub struct ConfigMessage {
  pub id:       String,
  pub data:     ConfigCommand,
  pub listener: Option<String>,
}

#[derive(Debug,Clone,PartialEq,Eq,Hash,Serialize,Deserialize)]
pub struct ConfigMessageAnswer {
  pub id:      String,
  pub status:  ConfigMessageStatus,
  pub message: String
}

fn main() {
  let matches = App::new("sozuctl")
                        .version(crate_version!())
                        .about("hot reconfigurable proxy")
                        .arg(Arg::with_name("config")
                            .short("c")
                            .long("config")
                            .value_name("FILE")
                            .help("Sets a custom config file")
                            .takes_value(true)
                            .required(true))
                        .subcommand(SubCommand::with_name("shutdown")
                                    .about("shuts down the proxy")
                                    .arg(Arg::with_name("hard").long("hard")
                                         .help("shuts down the proxy without waiting for connections to finish")))
                        .subcommand(SubCommand::with_name("state")
                                    .about("state management")
                                    .subcommand(SubCommand::with_name("save")
                                                .arg(Arg::with_name("file")
                                                    .short("f")
                                                    .long("file")
                                                    .value_name("state file")
                                                    .help("Save state to that file")
                                                    .takes_value(true)
                                                    .required(true)))
                                    .subcommand(SubCommand::with_name("load")
                                                .arg(Arg::with_name("file")
                                                    .short("f")
                                                    .long("file")
                                                    .value_name("state file")
                                                    .help("Save state to that file")
                                                    .takes_value(true)))
                                    .subcommand(SubCommand::with_name("dump")))
                        .get_matches();

  if let Some(matches) = matches.subcommand_matches("worker") {
    let fd  = matches.value_of("fd").expect("needs a file descriptor")
      .parse::<i32>().expect("the file descriptor must be a number");
    let id  = matches.value_of("id").expect("needs a worker id");
    let tag = matches.value_of("tag").expect("needs a configuration tag");

    return;
  }

  let config_file = matches.value_of("config").expect("required config file");

  let config = config::Config::load_from_path(config_file).expect("could not parse configuration file");
  let stream = UnixStream::connect(config.command_socket).expect("could not connect to the command unix socket");
  let mut channel: CommandChannel<ConfigMessage,ConfigMessageAnswer> = CommandChannel::new(stream, 10000, 20000);
  channel.set_nonblocking(false);

  match matches.subcommand() {
    ("shutdown", Some(sub)) => {
      let hard_shutdown = sub.is_present("hard");
    },
    ("state", Some(sub))    => {
      match sub.subcommand() {
        ("save", Some(state_sub)) => {
          let file = state_sub.value_of("file").expect("missing target file");
        },
        ("load", Some(state_sub)) => {
          let file = state_sub.value_of("file").expect("missing target file");
        },
        ("dump", _) => {
        },
        _                   => println!("unknown state management command")
      }
    },
    _                => println!("unknown subcommand")
  }

}
