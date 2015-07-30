use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Sender,Receiver};
use std::collections::HashMap;

#[derive(Debug,Clone,PartialEq,Eq,Hash)]
pub enum Tag {
  Msg
}

#[derive(Clone)]
pub enum Message {
  Subscribe(Tag, Sender<Message>),
  SubscribeOk,
  Msg(String)
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
      &Ok(Message::Msg(ref s)) => {
        println!("GOT MSG");
        if let &Some(v) = &self.senders.get(&Tag::Msg) {
          println!("GOT MSG 2");
          for tx in v {
            println!("GOT MSG 3");
            tx.send(Message::Msg(s.clone()));
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
  tx.send(Message::Subscribe(Tag::Msg, tx2));
  println!("BB");

  if let Ok(Message::SubscribeOk) = rx2.recv() {
    println!("successfully subscribed");
    let test = String::new() + "test";
    tx.send(Message::Msg(test.clone()));

    if let Ok(Message::Msg(s)) = rx2.recv() {
      assert_eq!(s, test);
    } else {
      assert!(false);
    }
  } else {
    assert!(false);
  }
}
