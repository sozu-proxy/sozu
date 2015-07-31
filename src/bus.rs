use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Sender,Receiver};
use std::collections::HashMap;

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct Acl {
    app_id: String,
    hostname: String,
    path_begin: String
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub struct Instance {
    app_id: String,
    ip_address: String,
    port: u16
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Command {
    AddAcl(Acl),
    RemoveAcl(Acl),
    AddInstance(Instance),
    RemoveInstance(Instance)
}

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Tag {
    AddAcl,
    RemoveAcl,
    AddInstance,
    RemoveInstance
}

#[derive(Clone)]
pub enum Message {
  Subscribe(Tag, Sender<Message>),
  SubscribeOk,
  Msg(Tag, Command)
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
  senders: HashMap<Tag, Vec<Sender<Message>>>
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
    match &self.rx.recv() {
      &Ok(Message::Subscribe(ref tag, ref tx)) => {
        self.subscribe(tag, tx);
        println!("SUBSCRIBED");
        return true;
      },
      &Ok(Message::Msg(ref t, ref c)) => {
        println!("GOT MSG");
        if let &Some(v) = &self.senders.get(&t) {
          println!("GOT MSG 2");
          for tx in v {
            println!("GOT MSG 3");
            tx.send(Message::Msg(t.clone(), c.clone()));
          }
        }
        return true;
      },
      &Err(_) => {
        println!("the bus's channel is closed, exiting");
        return false;
      }
      &Ok(_) => {
        println!("invalid message");
        // FIXME: maybe we should not stop if there's an invalid message
        return false;
      }
    }
  }

  fn subscribe(&mut self, t: &Tag, tx: &Sender<Message>) {
    println!("X");
    if ! &self.senders.contains_key(t) {
      let mut v = Vec::new();
      let t2 = t.clone();
      &self.senders.insert(t2, v);
      println!("Y");
    }
    if let Some(ref mut v) = self.senders.get_mut(t) {
      v.push(tx.clone());
      tx.send(Message::SubscribeOk);
      println!("Z");
    }
  }
}

#[test]
fn bus_test() {
  let tx = start_bus();

  let (tx2,rx2) = channel();
  println!("AA");
  tx.send(Message::Subscribe(Tag::AddAcl, tx2));
  println!("BB");

  if let Ok(Message::SubscribeOk) = rx2.recv() {
    println!("successfully subscribed");
    let tag = Tag::AddAcl;
    let command = Command::AddAcl(
      Acl {
        app_id: String::new() + "app_e74eb0d4-e01a-4a09-af46-7ecab7157d32",
        hostname: String::new() + "cltdl.fr",
        path_begin: String::new() + ""
      }
    );
    tx.send(Message::Msg(tag.clone(), command.clone()));

    if let Ok(Message::Msg(t, c)) = rx2.recv() {
      assert_eq!(t, tag);
      assert_eq!(c, command);
    } else {
      assert!(false);
    }
  } else {
    assert!(false);
  }
}
