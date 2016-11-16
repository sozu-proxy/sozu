use std::{iter,str};
use std::fmt::Debug;

pub type Key = Vec<u8>;
pub type KeyValue<K,V> = (K,V);

const BRANCH_FACTOR: usize = 4;

#[derive(Debug,PartialEq)]
pub struct TrieNode<V> {
  partial_key: Key,
  key_value:   Option<KeyValue<Key,V>>,
  children:    Vec<TrieNode<V>>,
}

#[derive(Debug,PartialEq)]
pub enum InsertResult {
  Ok,
  Existing,
  Failed
}

#[derive(Debug,PartialEq)]
pub enum RemoveResult {
  Ok,
  NotFound,
}

impl<V:Clone+Debug> TrieNode<V> {
  pub fn new(partial: &[u8], key: Key, value: V) -> TrieNode<V> {
    TrieNode {
      partial_key:    Vec::from(partial),
      key_value:      Some((key, value)),
      children:       vec!(),
    }
  }

  pub fn root() -> TrieNode<V> {
    TrieNode {
      partial_key:    vec!(),
      key_value:      None,
      children:       vec!(),
    }
  }

  pub fn split(&mut self, index:usize) {
    let key_value = self.key_value.take();
    let children  = self.children.drain(..).collect();

    let child = TrieNode {
      partial_key: (&self.partial_key[index..]).to_vec(),
      key_value:   key_value,
      children:    children,
    };

    self.children.push(child);
    self.partial_key = (&self.partial_key[..index]).to_vec();
  }

  pub fn insert(&mut self, key: Key, value: V) -> InsertResult {
    let res = self.insert_recursive(&key, &key, &value);
    assert_ne!(res, InsertResult::Failed);
    res
  }

  pub fn insert_recursive(&mut self, partial_key: &[u8], key: &Key, value: &V) -> InsertResult {
    assert_ne!(partial_key, &b""[..]);

    let pos = partial_key.iter().zip(self.partial_key.iter()).position(|(&a,&b)| a != b);

    match pos {
      Some(0) => InsertResult::Failed,
      Some(index) => {
        self.split(index);
        let new_child = TrieNode {
          partial_key: (&partial_key[index..]).to_vec(),
          key_value:   Some((key.clone(), value.clone())),
          children:    vec!(),
        };

        self.children.push(new_child);
        InsertResult::Ok
      },
      None    => {
        // we consumed the whole local partial key
        if partial_key.len() > self.partial_key.len() {
          for child in self.children.iter_mut() {
            match child.insert_recursive(&partial_key[self.partial_key.len()..], key, value) {
              InsertResult::Ok       => return InsertResult::Ok,
              InsertResult::Existing => return InsertResult::Existing,
              _ => {}
            }
          }

          // we could not insert in children
          let new_child = TrieNode {
            partial_key: (&partial_key[self.partial_key.len()..]).to_vec(),
            key_value:   Some((key.clone(), value.clone())),
            children:    vec!(),
          };

          self.children.push(new_child);
          InsertResult::Ok
        } else {
          // the partial key is the same as ours
          if self.key_value.is_some() {
            InsertResult::Existing
          } else {
            self.key_value = Some((key.clone(), value.clone()));
            InsertResult::Ok
          }
        }
      }
    }
  }

  pub fn remove(&mut self, key: &Key) -> RemoveResult {
    self.remove_recursive(key)
  }

  pub fn remove_recursive(&mut self, partial_key: &[u8]) -> RemoveResult {
    assert_ne!(partial_key, &b""[..]);
    let mut found_child:Option<usize> = None;

    // checking directly the children
    for (index, child) in self.children.iter_mut().enumerate() {

      let pos = partial_key.iter().zip(child.partial_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(i) => continue,
        None    => {
          if partial_key.len() > child.partial_key.len()  {
            let i = child.partial_key.len();
            return child.remove_recursive(&partial_key[i..]);
          } else if partial_key.len() == child.partial_key.len() {
            found_child = Some(index);
            break;
          } else {
            continue
          }
        }
      };
    }

    if let Some(index ) = found_child {
      self.children.remove(index);
      if self.key_value.is_some() {
        return RemoveResult::Ok;
      } else {
        //merging with the child
        if self.children.len() == 1 {
          let mut ch     = self.children.remove(0);
          self.key_value = ch.key_value.take();
          self.children  = ch.children;
          self.partial_key.extend(ch.partial_key);
        }
        // not handling the case of empty children vec
        // this case should only happen if it is the root node
        // otherwise, when we get to one last node and there is
        // no key_value, it is merged with the child node
        return RemoveResult::Ok
      }
    } else {
      RemoveResult::NotFound
    }
  }

  pub fn lookup(&self, partial_key: &[u8]) -> Option<&KeyValue<Key,V>> {
    assert_ne!(partial_key, &b""[..]);

    if partial_key.starts_with(&self.partial_key) {
      if partial_key.len() == self.partial_key.len() {
        return self.key_value.as_ref();
      } else {
        for child in self.children.iter() {
          let res = child.lookup(&partial_key[self.partial_key.len()..]);
          if res.is_some() {
            return res
          }
        }
        None
      }
    } else {
      None
    }
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup(&self, key: Key) -> Option<KeyValue<Key,V>> {
    None
  }

  pub fn print(&self) {
    self.print_recursive(0)
  }

  pub fn print_recursive(&self, indent:u8) {
    let raw_prefix:Vec<u8> = iter::repeat(' ' as u8).take(indent as usize).collect();
    let prefix = str::from_utf8(&raw_prefix).unwrap();

    if let Some((ref key, ref value)) = self.key_value {
    println!("{}{}: ({},{:?})", prefix, str::from_utf8(&self.partial_key).unwrap(),
      str::from_utf8(&key).unwrap(), value);
    } else {
    println!("{}{}: None", prefix, str::from_utf8(&self.partial_key).unwrap());
    }
    for child in self.children.iter() {
      child.print_recursive(indent+1);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn insert() {
    let mut root: TrieNode<u8> = TrieNode::root();
    root.print();

    assert_eq!(root.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    root.print();
    assert_eq!(root.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
    root.print();
    assert_eq!(root.insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);
    root.print();

    assert_eq!(root.lookup(&b"abce"[..]), Some(&((&b"abce"[..]).to_vec(), 2)));
    //assert!(false);
  }

  #[test]
  fn remove() {
    let mut root: TrieNode<u8> = TrieNode::root();

    assert_eq!(root.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(root.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
    assert_eq!(root.insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);

    let mut root2: TrieNode<u8> = TrieNode::root();

    assert_eq!(root2.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(root2.insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);

    println!("before remove");
    root.print();
    assert_eq!(root.remove(&Vec::from(&b"abce"[..])), RemoveResult::Ok);
    println!("after remove");
    root.print();
    println!("expected");
    root2.print();
    assert_eq!(root, root2);

    assert_eq!(root.remove(&Vec::from(&b"abgh"[..])), RemoveResult::Ok);
    println!("after remove");
    root.print();
    println!("expected");
    let mut root3: TrieNode<u8> = TrieNode::root();
    assert_eq!(root3.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    root3.print();
    assert_eq!(root, root3);

    assert!(false);
  }
}
