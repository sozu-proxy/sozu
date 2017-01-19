use sozu::channel::Channel;
use sozu_command::data::{ConfigCommand,ConfigMessage,ConfigMessageAnswer,ConfigMessageStatus};

fn generate_id() -> String {
  "hello".to_string()
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
