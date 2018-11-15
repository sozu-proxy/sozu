use std::{iter,str};
use std::fmt::Debug;

pub type Key = Vec<u8>;
pub type KeyValue<K,V> = (K,V);

#[derive(Debug,PartialEq)]
pub struct TrieNode<V> {
  key_value: Option<KeyValue<Key,V>>,
  keys:      Vec<Key>,
  children:  Vec<TrieNode<V>>,
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
  pub fn new(key: Key, value: V) -> TrieNode<V> {
    TrieNode {
      key_value:      Some((key, value)),
      keys:           vec!(),
      children:       vec!(),
    }
  }

  pub fn root() -> TrieNode<V> {
    TrieNode {
      key_value:      None,
      keys:           vec!(),
      children:       vec!(),
    }
  }

  pub fn insert(&mut self, key: Key, value: V) -> InsertResult {
    let res = self.insert_recursive(&key, &key, value);
    assert_ne!(res, InsertResult::Failed);
    //println!("adding {}", str::from_utf8(&key).unwrap());
    res
  }

  pub fn insert_recursive(&mut self, partial_key: &[u8], key: &Key, value: V) -> InsertResult {
    assert_ne!(partial_key, &b""[..]);

    let mut found = None;
    for (index, ref child_key) in self.keys.iter().enumerate() {
      let pos = partial_key.iter().zip(child_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(0) => continue,
        _       => found = Some(index),
      }
    }

    match found {
      None => {
        let new_child = TrieNode {
          key_value:   Some((key.clone(), value)),
          keys:        vec!(),
          children:    vec!(),
        };
        self.keys.push(partial_key.to_vec());
        self.children.push(new_child);
      },
      Some(index) => {
        let child_key = self.keys.remove(index);
        let mut child = self.children.remove(index);
        let pos = partial_key.iter().zip(child_key.iter()).position(|(&a,&b)| a != b);
        match pos {
          Some(i) => {
            let new_child = TrieNode {
              key_value:   Some((key.clone(), value)),
              keys:        vec!(),
              children:    vec!(),
            };

            let new_parent = TrieNode {
              key_value: None,
              keys:      vec!((&child_key[i..]).to_vec(), (&partial_key[i..]).to_vec()),
              children:  vec!(child, new_child)
            };

            self.keys.push((&partial_key[..i]).to_vec());
            self.children.push(new_parent);
          },
          None => {
            if partial_key.len() > child_key.len()  {
              let i = child_key.len();
              let res = child.insert_recursive(&partial_key[i..], key, value);
              self.keys.push((&partial_key[..i]).to_vec());
              self.children.push(child);
              return res;
            } else if partial_key.len() == child_key.len() {
              if child.key_value.is_some() {
                self.keys.push(child_key);
                self.children.push(child);
                return InsertResult::Existing;
              } else {
                child.key_value = Some((key.clone(), value));
                self.keys.push(child_key);
                self.children.push(child);
                return InsertResult::Ok;
              }
            } else {
              // the partial key is smaller, insert as parent
              let i = partial_key.len();
              let new_parent = TrieNode {
                key_value: Some((key.clone(), value)),
                keys: vec!((&child_key[i..]).to_vec()),
                children: vec!(child),
              };
              self.keys.push(partial_key.to_vec());
              self.children.push(new_parent);

              return InsertResult::Ok;
            }

          }
        }

      }
    }

    InsertResult::Ok
  }

  pub fn remove(&mut self, key: &Key) -> RemoveResult {
    self.remove_recursive(key)
  }

  pub fn remove_recursive(&mut self, partial_key: &[u8]) -> RemoveResult {
    assert_ne!(partial_key, &b""[..]);

    let mut found = None;

    for (index, ref child_key) in self.keys.iter().enumerate() {
      let pos = partial_key.iter().zip(child_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(_) => continue,
        None    => {
          if partial_key.len() > child_key.len()  {
            let i = child_key.len();
            return self.children[index].remove_recursive(&partial_key[i..]);
          } else if partial_key.len() == child_key.len() {
            found = Some(index);
            break;
          } else {
            continue
          }
        }
      };

    }

    if let Some(index) = found {
      if self.children[index].children.len() > 0 && self.children[index].key_value.is_some() {
        self.children[index].key_value = None;
        return RemoveResult::Ok;
      } else {
        self.keys.remove(index);
        self.children.remove(index);

        // we might get into a case where there's only one child, and we could merge it
        // with the parent, but this case is not handle right now
        return RemoveResult::Ok;
      }
    } else {
      RemoveResult::NotFound
    }
  }

  // specific version that will handle wildcard domains
  pub fn lookup(&self, key: &[u8]) -> Option<&KeyValue<Key,V>> {
    self.lookup_recursive(&key)
  }

  // specific version that will handle wildcard domains
  pub fn lookup_recursive(&self, partial_key: &[u8]) -> Option<&KeyValue<Key,V>> {
    assert_ne!(partial_key, &b""[..]);

    for (index, ref child_key) in self.keys.iter().enumerate() {
      let pos = partial_key.iter().zip(child_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(0) => continue,
        Some(i) => {
          // check for wildcard
          if i+1 == child_key.len() && child_key[i] == '*' as u8 {
            let c = '.' as u8;
            if (&partial_key[i..]).contains(&c) {
              return None;
            } else {
              return self.children[index].key_value.as_ref();
            }
          } else {
            return None;
          }
        },
        None    => {
          if partial_key.len() > child_key.len() {
            return self.children[index].lookup_recursive(&partial_key[child_key.len()..]);
          } else if partial_key.len() == child_key.len() {
            return self.children[index].key_value.as_ref();
          } else {
            return None;
          }
        }
      }
    }

    None
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
    if key.is_empty() {
      return None;
    }

    //println!("looking up: {}", str::from_utf8(key).unwrap());
    let mut partial_key = key.to_vec();
    partial_key.reverse();
    let res = self.domain_lookup_recursive(&partial_key);
    //println!(" => {:?}", res.map(|(k,v)| (str::from_utf8(k).unwrap().to_owned(), v)));
    res
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup_recursive(&self, partial_key: &[u8]) -> Option<&KeyValue<Key,V>> {
    assert_ne!(partial_key, &b""[..]);
    //println!("lookup '{}' in {:?}", str::from_utf8(partial_key).unwrap(), self.keys.iter().map(|k| str::from_utf8(k).unwrap()).collect::<Vec<_>>());

    // if we found a result with a wildcard, store it until the end of search
    // because we might find a more precise result
    let mut wildcard_res = None;

    for (index, ref child_key) in self.keys.iter().enumerate() {
      let pos = partial_key.iter().zip(child_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(0) => {
          if child_key[0] == '*' as u8 {
            let c = '.' as u8;
            if partial_key.contains(&c) {
              return None;
            } else {
              wildcard_res = self.children[index].key_value.as_ref();
            }
          }
        },
        Some(i) => {
          // check for wildcard
          if i+1 == child_key.len() && child_key[i] == '*' as u8 {
            let c = '.' as u8;
            if (&partial_key[i..]).contains(&c) {
              return None;
            } else {
              wildcard_res = self.children[index].key_value.as_ref();
            }
          }
        },
        None    => {
          if partial_key.len() > child_key.len() {
            return self.children[index].domain_lookup_recursive(&partial_key[child_key.len()..]);
          } else if partial_key.len() == child_key.len() {
            return self.children[index].key_value.as_ref();
          } else {
            return None;
          }
        }
      }
    }

    wildcard_res
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup_mut(&mut self, key: &[u8]) -> Option<&mut KeyValue<Key,V>> {
    let mut partial_key = key.to_vec();
    partial_key.reverse();
    self.domain_lookup_mut_recursive(&partial_key)
  }

  // specific version that will handle wildcard domains
  pub fn domain_lookup_mut_recursive(&mut self, partial_key: &[u8]) -> Option<&mut KeyValue<Key,V>> {
    assert_ne!(partial_key, &b""[..]);

    // if we found a result with a wildcard, store it until the end of search
    // because we might find a more precise result
    let mut wildcard_res = None;

    for (index, ref child_key) in self.keys.iter().enumerate() {
      let pos = partial_key.iter().zip(child_key.iter()).position(|(&a,&b)| a != b);
      match pos {
        Some(0) => if child_key[0] == b'*' {
          let c = b'.';
          if partial_key.contains(&c) {
            return None;
          } else {
            wildcard_res = Some(index);//self.children[index].key_value.as_mut();
          }
        },
        Some(i) => {
          // check for wildcard
          if i+1 == child_key.len() && child_key[i] == b'*' {
            let c = b'.';
            if (&partial_key[i..]).contains(&c) {
              return None;
            } else {
              wildcard_res = Some(index);//self.children[index].key_value.as_mut();
            }
          }
        },
        None    => {
          if partial_key.len() > child_key.len() {
            return self.children[index].domain_lookup_mut_recursive(&partial_key[child_key.len()..]);
          } else if partial_key.len() == child_key.len() {
            return self.children[index].key_value.as_mut();
          } else {
            return None;
          }
        }
      }
    }

    wildcard_res.and_then(move |index| self.children[index].key_value.as_mut())
  }

  pub fn print(&self) {
    self.print_recursive(b"", 0)
  }

  pub fn print_recursive(&self, partial_key: &[u8], indent:u8) {
    let raw_prefix:Vec<u8> = iter::repeat(' ' as u8).take(2*indent as usize).collect();
    let prefix = str::from_utf8(&raw_prefix).unwrap();

    if let Some((ref key, ref value)) = self.key_value {
    println!("{}{}: ({},{:?})", prefix, str::from_utf8(partial_key).unwrap(),
      str::from_utf8(&key).unwrap(), value);
    } else {
    println!("{}{}: None", prefix, str::from_utf8(partial_key).unwrap());
    }
    for (ref child_key, ref child) in self.keys.iter().zip(self.children.iter()) {
      child.print_recursive(&child_key, indent+1);
    }
  }
}
#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

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
    /*assert_eq!(root, root2);

    assert_eq!(root.remove(&Vec::from(&b"abgh"[..])), RemoveResult::Ok);
    println!("after remove");
    root.print();
    println!("expected");
    let mut root3: TrieNode<u8> = TrieNode::root();
    assert_eq!(root3.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    root3.print();
    assert_eq!(root, root3);
    */
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
    assert_eq!(root.domain_insert(Vec::from(&b"test.example.com"[..]), 2), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"*.alldomains.org"[..]), 3), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"alldomains.org"[..]), 4), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"pouet.alldomains.org"[..]), 5), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"hello.com"[..]), 6), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"*.hello.com"[..]), 7), InsertResult::Ok);
    root.print();

    assert_eq!(root.domain_lookup(&b"example.com"[..]), None);
    assert_eq!(root.domain_lookup(&b"blah.test.example.com"[..]), None);
    assert_eq!(root.domain_lookup(&b"www.example.com"[..]), Some(&((&b"www.example.com"[..]).to_vec(), 1)));
    assert_eq!(root.domain_lookup(&b"alldomains.org"[..]), Some(&((&b"alldomains.org"[..]).to_vec(), 4)));
    assert_eq!(root.domain_lookup(&b"test.hello.com"[..]), Some(&((&b"*.hello.com"[..]).to_vec(), 7)));
    assert_eq!(root.domain_lookup(&b"test.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"hello.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"pouet.alldomains.org"[..]), Some(&((&b"pouet.alldomains.org"[..]).to_vec(), 5)));
    assert_eq!(root.domain_lookup(&b"blah.test.alldomains.org"[..]), None);

    assert_eq!(root.domain_remove(&Vec::from(&b"alldomains.org"[..])), RemoveResult::Ok);
    println!("after remove");
    root.print();
    assert_eq!(root.domain_lookup(&b"alldomains.org"[..]), None);
    assert_eq!(root.domain_lookup(&b"test.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"hello.alldomains.org"[..]), Some(&((&b"*.alldomains.org"[..]).to_vec(), 3)));
    assert_eq!(root.domain_lookup(&b"pouet.alldomains.org"[..]), Some(&((&b"pouet.alldomains.org"[..]).to_vec(), 5)));
    assert_eq!(root.domain_lookup(&b"test.hello.com"[..]), Some(&((&b"*.hello.com"[..]).to_vec(), 7)));
    assert_eq!(root.domain_lookup(&b"blah.test.alldomains.org"[..]), None);
  }

  fn hm_insert(h: HashMap<String, u32>) -> bool {
    let mut root: TrieNode<u32> = TrieNode::root();

    for (k, v) in h.iter() {
      if k.is_empty() {
        continue;
      }

      //println!("inserting key: '{}', value: '{}'", k, v);
      //assert_eq!(root.domain_insert(Vec::from(k.as_bytes()), *v), InsertResult::Ok);
      assert_eq!(root.insert(Vec::from(k.as_bytes()), *v), InsertResult::Ok, "could not insert ({}, {})", k, v);
      //root.print();
    }

    //root.print();
    for (k, v) in h.iter() {
      if k.is_empty() {
        continue;
      }

      //match root.domain_lookup(k.as_bytes()) {
      match root.lookup(k.as_bytes()) {
        None => {
          println!("did not find key '{}'", k);
          return false;
        }
        Some(&(ref k1,v1)) => if k.as_bytes() != &k1[..] || *v != v1 {
          println!("request ({}, {}), got ({}, {})", k, v, str::from_utf8(&k1[..]).unwrap(), v1);
          return false;
        }
      }
    }

    true
  }

  quickcheck! {
    fn qc_insert(h: HashMap<String, u32>) -> bool {
      hm_insert(h)
    }
  }

  #[test]
  fn insert_disappearing_tree() {
    let h: HashMap<String, u32> = [
      (String::from("\n\u{3}"), 0),
      (String::from("\n\u{0}"), 1),
      (String::from("\n"), 2)
    ].iter().cloned().collect();
    assert!(hm_insert(h));
  }
}
