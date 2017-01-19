use sozu::channel::Channel;
use sozu::messages::Order;
use sozu_command::config::Config;
use sozu_command::data::{ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus};

use std::iter::FromIterator;
use std::collections::HashMap;
use rand::{thread_rng, Rng};

fn generate_id() -> String {
  let s: String = thread_rng().gen_ascii_chars().take(6).collect();
  format!("ID-{}", s)
}

pub fn save_state(channel: &mut Channel<ConfigMessage,ConfigMessageAnswer>, path: &str) {
  let id = generate_id();
  channel.write_message(&ConfigMessage {
    id:    id.clone(),
    data:  ConfigCommand::SaveState(path.to_string()),
    proxy: None,
  });

  match channel.read_message() {
    None          => println!("the proxy didn't answer"),
    Some(message) => {
      if id != message.id {
        println!("received message with invalid id: {:?}", message);
        return;
      }
      match message.status {
        ConfigMessageStatus::Processing => {
          // do nothing here
          // for other messages, we would loop over read_message
          // until an error or ok message was sent
        },
        ConfigMessageStatus::Error => {
          println!("could not save proxy state: {}", message.message);
        },
        ConfigMessageStatus::Ok => {
          println!("Proxy state saved to {}", path);
        }
      }
    }
  }
}

pub fn load_state(channel: &mut Channel<ConfigMessage,ConfigMessageAnswer>, path: &str) {
  let id = generate_id();
  channel.write_message(&ConfigMessage {
    id:    id.clone(),
    data:  ConfigCommand::LoadState(path.to_string()),
    proxy: None,
  });

  match channel.read_message() {
    None          => println!("the proxy didn't answer"),
    Some(message) => {
      if id != message.id {
        println!("received message with invalid id: {:?}", message);
        return;
      }
      match message.status {
        ConfigMessageStatus::Processing => {
          // do nothing here
          // for other messages, we would loop over read_message
          // until an error or ok message was sent
        },
        ConfigMessageStatus::Error => {
          println!("could not save proxy state: {}", message.message);
        },
        ConfigMessageStatus::Ok => {
          println!("Proxy state saved to {}", path);
        }
      }
    }
  }
}

pub fn dump_state(channel: &mut Channel<ConfigMessage,ConfigMessageAnswer>) {
  let id = generate_id();
  channel.write_message(&ConfigMessage {
    id:    id.clone(),
    data:  ConfigCommand::DumpState,
    proxy: None,
  });

  match channel.read_message() {
    None          => println!("the proxy didn't answer"),
    Some(message) => {
      if id != message.id {
        println!("received message with invalid id: {:?}", message);
        return;
      }
      match message.status {
        ConfigMessageStatus::Processing => {
          // do nothing here
          // for other messages, we would loop over read_message
          // until an error or ok message was sent
        },
        ConfigMessageStatus::Error => {
          println!("could not dump proxy state: {}", message.message);
        },
        ConfigMessageStatus::Ok => {
          println!("Proxy state:\n{}", message.message);
        }
      }
    }
  }
}

pub fn soft_stop(channel: &mut Channel<ConfigMessage,ConfigMessageAnswer>, config: &Config) {
  let mut tags: HashMap<String,String> = HashMap::from_iter(config.proxies.keys().map(|tag| {
    println!("shutting down proxy \"{}\"", tag);
    let id = generate_id();
    channel.write_message(&ConfigMessage {
      id:    id.clone(),
      data:  ConfigCommand::ProxyConfiguration(Order::SoftStop),
      proxy: Some(tag.clone()),
    });
    (id, tag.clone())
  }));

  loop {
    if tags.is_empty() {
      println!("all proxies shut down");
      break;
    }

    match channel.read_message() {
      None          => println!("the proxy didn't answer"),
      Some(message) => {
        if !tags.contains_key(&message.id) {
          println!("received message with invalid id: {:?}", message);
          return;
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            tags.get(&message.id).map(|tag| println!("Proxy {} is processing: {}", tag, message.message));
          },
          ConfigMessageStatus::Error => {
            println!("could not stop the proxy: {}", message.message);
          },
          ConfigMessageStatus::Ok => {
            if let Some(tag) = tags.remove(&message.id) {
              println!("Proxy {} shut down: {}", tag, message.message);
            }
          }
        }
      }
    }
  }
}

pub fn hard_stop(channel: &mut Channel<ConfigMessage,ConfigMessageAnswer>, config: &Config) {
  let mut tags: HashMap<String,String> = HashMap::from_iter(config.proxies.keys().map(|tag| {
    println!("shutting down proxy \"{}\"", tag);
    let id = generate_id();
    channel.write_message(&ConfigMessage {
      id:    id.clone(),
      data:  ConfigCommand::ProxyConfiguration(Order::HardStop),
      proxy: Some(tag.clone()),
    });
    (id, tag.clone())
  }));


  loop {
    if tags.is_empty() {
      println!("all proxies shut down");
      break;
    }

    match channel.read_message() {
      None          => println!("the proxy didn't answer"),
      Some(message) => {
        if !tags.contains_key(&message.id) {
          println!("received message with invalid id: {:?}", message);
          return;
        }
        match message.status {
          ConfigMessageStatus::Processing => {
            tags.get(&message.id).map(|tag| println!("Proxy {} is processing: {}", tag, message.message));
          },
          ConfigMessageStatus::Error => {
            println!("could not stop the proxy: {}", message.message);
          },
          ConfigMessageStatus::Ok => {
            if let Some(tag) = tags.remove(&message.id) {
              println!("Proxy {} shut down: {}", tag, message.message);
            }
          }
        }
      }
    }
  }
}
