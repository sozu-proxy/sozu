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

impl<V:Debug> TrieNode<V> {
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
    println!("adding {}", str::from_utf8(&key).unwrap());
    let res = self.insert_recursive(&key, &key, value);
    assert_ne!(res, InsertResult::Failed);
    res
  }

  pub fn insert_recursive(&mut self, partial_key: &[u8], key: &Key, value: V) -> InsertResult {
    assert_ne!(partial_key, &b""[..]);

    // checking directly the children
    for (index, child) in self.children.iter_mut().enumerate() {
      let pos = partial_key.iter().zip(child.partial_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(0) => continue,
        Some(i) => {
          child.split(i);
          let new_child = TrieNode {
            partial_key: (&partial_key[i..]).to_vec(),
            key_value:   Some((key.clone(), value)),
            children:    vec!(),
          };

          child.children.push(new_child);
          return InsertResult::Ok;
        },
        None    => {
          if partial_key.len() > child.partial_key.len()  {
            let i = child.partial_key.len();
            return child.insert_recursive(&partial_key[i..], key, value);
          } else if partial_key.len() == child.partial_key.len() {
            if child.key_value.is_some() {
              return InsertResult::Existing;
            } else {
              child.key_value = Some((key.clone(), value));
              return InsertResult::Ok;
            }
          } else {
            // the partial key is smaller, insert as parent
            child.split(partial_key.len());
            child.key_value = Some((key.clone(), value));
            return InsertResult::Ok;
          }
        }
      };
    }

    let new_child = TrieNode {
      partial_key: partial_key.to_vec(),
      key_value:   Some((key.clone(), value)),
      children:    vec!(),
    };

    self.children.push(new_child);
    InsertResult::Ok
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

    if let Some(index) = found_child {
      if self.children[index].children.len() > 0 && self.children[index].key_value.is_some() {
        self.children[index].key_value = None;
        return RemoveResult::Ok;
      } else {
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
  pub fn domain_insert(&mut self, key: Key, value: V) -> InsertResult {
    let mut partial_key = key.clone();
    partial_key.reverse();
    self.insert_recursive(&partial_key, &key, value)
  }

  // specific version that will handle wildcard domains
  pub fn domain_remove(&mut self, key: &Key) -> RemoveResult {
    let mut partial_key = key.clone();
    partial_key.reverse();
    self.remove_recursive(&partial_key)
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup(&self, key: &[u8]) -> Option<&KeyValue<Key,V>> {
    let mut partial_key = key.to_vec();
    partial_key.reverse();
    self.domain_lookup_recursive(&partial_key)
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup_recursive(&self, partial_key: &[u8]) -> Option<&KeyValue<Key,V>> {
    assert_ne!(partial_key, &b""[..]);
    let pos = partial_key.iter().zip(self.partial_key.iter()).position(|(&a,&b)| a != b);
    //info!("lookup at level: {}, testing {}", str::from_utf8(&self.partial_key).unwrap(),
    //  str::from_utf8(partial_key).unwrap());

    match pos {
      Some(i) => {
        // check for wildcard
        if i+1 == self.partial_key.len() && self.partial_key[i] == '*' as u8 {
          let c = '.' as u8;
          if (&partial_key[i..]).contains(&c) {
            None
          } else {
            self.key_value.as_ref()
          }
        } else {
          None
        }
      },
      None    => {
        if partial_key.len() > self.partial_key.len() {
          for child in self.children.iter() {
            let res = child.domain_lookup_recursive(&partial_key[self.partial_key.len()..]);
            if res.is_some() {
              return res
            }
          }
          None
        } else if partial_key.len() == self.partial_key.len() {
          self.key_value.as_ref()
        } else {
          None
        }

      }
    }
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup_mut(&mut self, key: &[u8]) -> Option<&mut KeyValue<Key,V>> {
    let mut partial_key = key.to_vec();
    partial_key.reverse();
    self.domain_lookup_recursive_mut(&partial_key)
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup_recursive_mut(&mut self, partial_key: &[u8]) -> Option<&mut KeyValue<Key,V>> {
    assert_ne!(partial_key, &b""[..]);
    let pos = partial_key.iter().zip(self.partial_key.iter()).position(|(&a,&b)| a != b);
    //info!("lookup at level: {}, testing {}", str::from_utf8(&self.partial_key).unwrap(),
    //  str::from_utf8(partial_key).unwrap());

    match pos {
      Some(i) => {
        // check for wildcard
        if i+1 == self.partial_key.len() && self.partial_key[i] == '*' as u8 {
          let c = '.' as u8;
          if (&partial_key[i..]).contains(&c) {
            None
          } else {
            self.key_value.as_mut()
          }
        } else {
          None
        }
      },
      None    => {
        if partial_key.len() > self.partial_key.len() {
          for child in self.children.iter_mut() {
            let res = child.domain_lookup_recursive_mut(&partial_key[self.partial_key.len()..]);
            if res.is_some() {
              return res
            }
          }
          None
        } else if partial_key.len() == self.partial_key.len() {
          self.key_value.as_mut()
        } else {
          None
        }

      }
    }
  }

  pub fn print(&self) {
    self.print_recursive(0)
  }

  pub fn print_recursive(&self, indent:u8) {
    let raw_prefix:Vec<u8> = iter::repeat(' ' as u8).take(2*indent as usize).collect();
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
  }

  #[test]
  fn add_child_to_leaf() {
    let mut root: TrieNode<u8> = TrieNode::root();

    assert_eq!(root.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(root.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
    assert_eq!(root.insert(Vec::from(&b"abc"[..]), 3), InsertResult::Ok);

    root.print();

    let mut root2: TrieNode<u8> = TrieNode::root();

    assert_eq!(root2.insert(Vec::from(&b"abc"[..]), 3), InsertResult::Ok);
    assert_eq!(root2.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(root2.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);

    root2.print();
    assert_eq!(root2.remove(&Vec::from(&b"abc"[..])), RemoveResult::Ok);

    let mut expected: TrieNode<u8> = TrieNode::root();

    assert_eq!(expected.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(expected.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);

    println!("after remove");
    root2.print();
    println!("expected");
    expected.print();
    assert_eq!(root2, expected);

  }

  #[test]
  fn domains() {
    let mut root: TrieNode<u8> = TrieNode::root();

    assert_eq!(root.domain_insert(Vec::from(&b"www.example.com"[..]), 1), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"test.example.com"[..]), 2), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"*.alldomains.org"[..]), 3), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"alldomains.org"[..]), 4), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"hello.com"[..]), 5), InsertResult::Ok);
    root.print();

    assert_eq!(root.domain_lookup(&b"example.com"[..]), None);
    assert_eq!(root.domain_lookup(&b"blah.test.example.com"[..]), None);
    assert_eq!(root.domain_lookup(&b"www.example.com"[..]), Some(&((&b"www.example.com"[..]).to_vec(), 1)));
    assert_eq!(root.domain_lookup(&b"alldomains.org"[..]), Some(&((&b"alldomains.org"[..]).to_vec(), 4)));
    assert_eq!(root.domain_lookup(&b"test.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"hello.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"blah.test.alldomains.org"[..]), None);

    assert_eq!(root.domain_remove(&Vec::from(&b"alldomains.org"[..])), RemoveResult::Ok);
    println!("after remove");
    root.print();
    assert_eq!(root.domain_lookup(&b"alldomains.org"[..]), None);
    assert_eq!(root.domain_lookup(&b"test.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"hello.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"blah.test.alldomains.org"[..]), None);
  }
}
