use std::thread::{self};
use std::sync::mpsc::{channel,Sender,Receiver};
use std::collections::HashMap;
use std::fmt;

use messages::{Command, Topic};

#[derive(Clone)]
pub enum Message {
  Subscribe(Topic, Sender<Message>),
  SubscribeOk,
  Msg(Command)
}

impl fmt::Display for Message {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
      match *self {
        Message::Subscribe(_, _) => write!(f, "Subscribe"),
        Message::SubscribeOk     => write!(f, "SubscribeOk"),
        Message::Msg(ref c)      => {
          (write!(f, "{:?}", c.get_topics())).and(
            write!(f, "{:?}", c))
        }
      }
  }
}

pub fn start_bus() -> Sender<Message> {
  let (tx,rx) = channel::<Message>();

  thread::spawn(move|| {
    let mut bus = Bus {
      rx: rx,
      senders: HashMap::new()
    };

    bus.loop_message();
  });
  tx
}

struct Bus {
  rx: Receiver<Message>,
  senders: HashMap<Topic, Vec<Sender<Message>>>
}

impl Bus {
  fn loop_message(&mut self) {
    loop {
      if ! self.handle_message() {
        break;
      }
    }
  }

  fn handle_message(&mut self) -> bool {
    match self.rx.recv() {
      Ok(Message::Subscribe(ref tag, ref tx)) => {
        self.subscribe(tag, tx);
        println!("SUBSCRIBED");
        true
      },
      Ok(Message::Msg(ref c)) => {
        println!("GOT MSG");
        let topics = c.get_topics();

        for t in topics {
          if let &Some(v) = &self.senders.get(&t) {
            println!("GOT MSG 2");
            for tx in v {
              println!("GOT MSG 3");
              // ToDo use result?
              let _ = tx.send(Message::Msg(c.clone()));
              println!("{:?}", c);
            }
          }
        }
        true
      },
      Err(_) => {
        println!("the bus's channel is closed, exiting");
        false
      }
      Ok(_) => {
        println!("invalid message");
        // FIXME: maybe we should not stop if there's an invalid message
        false
      }
    }
  }

  fn subscribe(&mut self, t: &Topic, tx: &Sender<Message>) {
    println!("X");
    if ! &self.senders.contains_key(t) {
      let v = Vec::new();
      let t2 = t.clone();
      &self.senders.insert(t2, v);
      println!("Y");
    }
    if let Some(ref mut v) = self.senders.get_mut(t) {
      v.push(tx.clone());
      // ToDo use result?
      let _ = tx.send(Message::SubscribeOk);
      println!("Z");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use messages::{Command,HttpFront,Topic};
  use std::sync::mpsc::{channel};

  #[test]
  fn bus_test() {
    let tx = start_bus();

    let (tx2,rx2) = channel();
    println!("AA");
    let _ = tx.send(Message::Subscribe(Topic::HttpProxyConfig, tx2));
    println!("BB");

    if let Ok(Message::SubscribeOk) = rx2.recv() {
      println!("successfully subscribed");
      let command = Command::AddHttpFront(
        HttpFront {
          app_id: String::new() + "app_e74eb0d4-e01a-4a09-af46-7ecab7157d32",
          hostname: String::new() + "cltdl.fr",
          path_begin: String::new() + "",
          port: 8080
        }
      );
      let _ = tx.send(Message::Msg(command.clone()));

      if let Ok(Message::Msg(c)) = rx2.recv() {
        assert_eq!(c, command);
      } else {
        assert!(false);
      }
    } else {
      assert!(false);
    }
  }
}

