use std::{iter, str, fmt::Debug};
use hashbrown::HashMap;
use regex::bytes::Regex;

pub type Key = Vec<u8>;
pub type KeyValue<K,V> = (K,V);

#[derive(Debug,PartialEq)]
pub enum InsertResult {
  Ok,
  Existing,
  Failed
}

#[derive(Debug,PartialEq)]
pub enum RemoveResult {
  Ok,
  NotFound
}

fn find_last_dot(input: &[u8]) -> Option<usize> {
  //println!("find_last_dot: input = {}", from_utf8(input).unwrap());
  for i in (0..input.len()).rev() {
    //println!("input[{}] -> {}", i, input[i] as char);
    if input[i] == b'.' {
      return Some(i);
    }
  }

  None
}

fn find_last_slash(input: &[u8]) -> Option<usize> {
  //println!("find_last_dot: input = {}", from_utf8(input).unwrap());
  for i in (0..input.len()).rev() {
    //println!("input[{}] -> {}", i, input[i] as char);
    if input[i] == b'/' {
      return Some(i);
    }
  }

  None
}

#[derive(Debug)]
pub struct TrieNode<V> {
  key_value: Option<KeyValue<Key,V>>,
  wildcard:  Option<KeyValue<Key, V>>,
  children:  HashMap<Key, TrieNode<V>>,
  regexps:   Vec<(Regex, TrieNode<V>)>,
}

impl<V: PartialEq> std::cmp::PartialEq for TrieNode<V> {
  fn eq(&self, other: &Self) -> bool {
    self.key_value == other.key_value &&
      self.wildcard == other.wildcard &&
      self.children == other.children &&
      self.regexps.len() == other.regexps.len() &&
      self.regexps.iter().zip(other.regexps.iter()).fold(true, |b, (left, right)| {
        b && left.0.as_str() == right.0.as_str() && left.1 == right.1
      })
  }
}

impl<V:Debug+Clone> TrieNode<V> {
  pub fn new(key: Key, value: V) -> TrieNode<V> {
    TrieNode {
      key_value: Some((key, value)),
      wildcard:  None,
      children:  HashMap::new(),
      regexps:   Vec::new(),
    }
  }

  pub fn wildcard(key: Key, value: V) -> TrieNode<V> {
    TrieNode {
      key_value: None,
      wildcard:  Some((key, value)),
      children:  HashMap::new(),
      regexps:   Vec::new(),
    }
  }

  pub fn root() -> TrieNode<V> {
    TrieNode {
      key_value: None,
      wildcard:  None,
      children:  HashMap::new(),
      regexps:   Vec::new(),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.key_value.is_none() && self.wildcard.is_none()  && self.children.is_empty()
  }

  pub fn insert(&mut self, key: Key, value: V) -> InsertResult {
    //println!("insert: key == {}", std::str::from_utf8(&key).unwrap());
    if key.is_empty() {
      return InsertResult::Failed;
    }
    if &key[..] == &b"."[..] {
      return InsertResult::Failed;
    }

    let res = self.insert_recursive(&key, &key, value);
    assert_ne!(res, InsertResult::Failed);
    res
  }

  pub fn insert_recursive(&mut self, partial_key: &[u8], key: &Key, value: V) -> InsertResult {
    println!("insert_rec: key == {}", std::str::from_utf8(partial_key).unwrap());
    assert_ne!(partial_key, &b""[..]);

    if partial_key[partial_key.len() - 1] == b'/' {
      let pos = find_last_slash(&partial_key[..partial_key.len() - 1]);

      if let Some(pos) = pos {
        if pos > 0 && partial_key[pos - 1] != b'.' {
          return InsertResult::Failed;
        }

        if let Ok(s) = str::from_utf8(&partial_key[pos+1..partial_key.len() - 1]) {
          for t in self.regexps.iter() {
            if t.0.as_str() == s {
              return InsertResult::Existing;
            }
          }

          if let Ok(r) = Regex::new(s) {
            if pos > 0 {
              let mut node = TrieNode::root();
              let pos = pos - 1;

              let res = node.insert_recursive(&partial_key[..pos], key, value);

              if res == InsertResult::Ok {
                self.regexps.push((r, node));
              }

              return res;
            } else {
              let node = TrieNode::new(key.to_vec(), value);
              self.regexps.push((r, node));
              return InsertResult::Ok;
            }
          }
        }
      }

      return InsertResult::Failed;
    }

    let pos = find_last_dot(partial_key);
    match pos {
      None => {
        if self.children.contains_key(partial_key) {
          InsertResult::Existing
        } else {
          if partial_key == &b"*"[..] {
            if self.wildcard.is_some() {
              InsertResult::Existing
            } else {
              self.wildcard = Some((key.to_vec(), value));
              InsertResult::Ok
            }
          } else {
            let node = TrieNode::new(key.to_vec(), value);
            self.children.insert(partial_key.to_vec(), node);
            InsertResult::Ok
          }
        }
      }
      Some(pos) => {
        if let Some(child) = self.children.get_mut(&partial_key[pos..]) {
          return child.insert_recursive(&partial_key[..pos], key, value);
        }

        let mut node = TrieNode::root();
        let res = node.insert_recursive(&partial_key[..pos], key, value);

        if res == InsertResult::Ok {
          self.children.insert((&partial_key[pos..]).to_vec(), node);
        }

        res
      }
    }

  }

  pub fn remove(&mut self, key: &Key) -> RemoveResult {
    self.remove_recursive(key)
  }

  pub fn remove_recursive(&mut self, partial_key: &[u8]) -> RemoveResult {
    //println!("remove: key == {}", std::str::from_utf8(partial_key).unwrap());

    if partial_key.len() == 0 {
      if self.key_value.is_some() {
        self.key_value = None;
        return RemoveResult::Ok;
      } else {
        return RemoveResult::NotFound;
      }
    }

    if partial_key == &b"*"[..] {
      if self.wildcard.is_some() {
        self.wildcard = None;
        return RemoveResult::Ok;
      } else {
        return RemoveResult::NotFound;
      }
    }

    if partial_key[partial_key.len() - 1] == b'/' {
      let pos = find_last_slash(&partial_key[..partial_key.len() - 1]);

      if let Some(pos) = pos {

        if let Ok(s) = str::from_utf8(&partial_key[pos+1..partial_key.len() - 1]) {
          let len = self.regexps.len();
          // FIXME: we might have multipe entries with the same regex
          self.regexps.retain(|(r, _)| r.as_str() != s);
          if len > self.regexps.len() {
           return  RemoveResult::Ok;
          }
        }
      }

      return RemoveResult::NotFound;
    }


    let pos = find_last_dot(partial_key);
    let (prefix, suffix) = match pos {
      None => (&b""[..], partial_key),
      Some(pos) => (&partial_key[..pos], &partial_key[pos..]),
    };
    //println!("remove: prefix|suffix: {} | {}", std::str::from_utf8(prefix).unwrap(), std::str::from_utf8(suffix).unwrap());

    match self.children.get_mut(suffix) {
      Some(child) => {
        match child.remove_recursive(prefix) {
          RemoveResult::NotFound => return RemoveResult::NotFound,
          RemoveResult::Ok => {
            if !child.is_empty() {
              return RemoveResult::Ok;
            }
          }
        }
      }
      None => {
        return RemoveResult::NotFound
      }
    }

    // if we reach here, that means we called remove_recursive on a child
    // and the child is now empty
    self.children.remove(suffix);
    RemoveResult::Ok
  }

  pub fn lookup(&self, partial_key: &[u8]) -> Option<&KeyValue<Key,V>> {
    println!("lookup: key == {}", std::str::from_utf8(partial_key).unwrap());

    if partial_key.len() == 0 {
      return self.key_value.as_ref();
    }

    let pos = find_last_dot(partial_key);
    let (prefix, suffix) = match pos {
      None => (&b""[..], partial_key),
      Some(pos) => (&partial_key[..pos], &partial_key[pos..]),
    };
    //println!("lookup: prefix|suffix: {} | {}", std::str::from_utf8(prefix).unwrap(), std::str::from_utf8(suffix).unwrap());

    match self.children.get(suffix) {
      Some(child) => child.lookup(prefix),
      None => {
        println!("no child found, testing wildcard and regexps");
        self.print();

        if prefix.is_empty() && self.wildcard.is_some() {
          //println!("no dot, wildcard applies");
          self.wildcard.as_ref()
        } else {
          //println!("there's still a subdomain, wildcard does not apply");

          for (ref r, ref child) in self.regexps.iter() {
            let s = if suffix[0] == b'.' {
              &suffix[1..]
            } else {
              suffix
            };
            println!("testing regexp: {} on suffix {}", r.as_str(), str::from_utf8(s).unwrap());

            if r.is_match(s) {
              println!("matched");
              return child.lookup(prefix);
            }
          }

          None
        }
      }
    }
  }

  pub fn lookup_mut(&mut self, partial_key: &[u8]) -> Option<&mut KeyValue<Key,V>> {
    println!("lookup: key == {}", std::str::from_utf8(partial_key).unwrap());

    if partial_key.len() == 0 {
      return self.key_value.as_mut();
    }

    let pos = find_last_dot(partial_key);
    let (prefix, suffix) = match pos {
      None => (&b""[..], partial_key),
      Some(pos) => (&partial_key[..pos], &partial_key[pos..]),
    };
    //println!("lookup: prefix|suffix: {} | {}", std::str::from_utf8(prefix).unwrap(), std::str::from_utf8(suffix).unwrap());

    match self.children.get_mut(suffix) {
      Some(child) => child.lookup_mut(prefix),
      None => {
        println!("no child found, testing wildcard and regexps");

        if prefix.is_empty() && self.wildcard.is_some() {
          //println!("no dot, wildcard applies");
          self.wildcard.as_mut()
        } else {
          //println!("there's still a subdomain, wildcard does not apply");

          for (ref r, ref mut child) in self.regexps.iter_mut() {
            let s = if suffix[0] == b'.' {
              &suffix[1..]
            } else {
              suffix
            };
            println!("testing regexp: {} on suffix {}", r.as_str(), str::from_utf8(s).unwrap());

            if r.is_match(s) {
              println!("matched");
              return child.lookup_mut(prefix);
            }
          }

          None
        }
      }
    }
  }

  pub fn print(&self) {
    self.print_recursive(b"", 0)
  }

  pub fn print_recursive(&self, partial_key: &[u8], indent:u8) {
    let raw_prefix:Vec<u8> = iter::repeat(' ' as u8).take(2*indent as usize).collect();
    let prefix = str::from_utf8(&raw_prefix).unwrap();

    print!("{}{}: ", prefix, str::from_utf8(partial_key).unwrap());
    if let Some((ref key, ref value)) = self.key_value {
      print!("({}, {:?}) | ", str::from_utf8(&key).unwrap(), value);
    } else {
      print!("None | ");
    }

    if let Some((ref key, ref value)) = self.wildcard {
      println!("({}, {:?})", str::from_utf8(&key).unwrap(), value);
    } else {
      println!("None");
    }

    for (ref child_key, ref child) in self.children.iter() {
      child.print_recursive(child_key, indent+1);
    }

    for (ref regexp, ref child) in self.regexps.iter() {
      //print!("{}{}:", prefix, regexp.as_str());
      child.print_recursive(regexp.as_str().as_bytes(), indent+1);
    }

  }

  pub fn domain_insert(&mut self, key: Key, value: V) -> InsertResult {
    self.insert(key, value)
  }

  pub fn domain_remove(&mut self, key: &Key) -> RemoveResult {
    self.remove(key)
  }

  pub fn domain_lookup(&self, key: &[u8]) -> Option<&KeyValue<Key,V>> {
    self.lookup(key)
  }

  pub fn domain_lookup_mut(&mut self, key: &[u8]) -> Option<&mut KeyValue<Key,V>> {
    self.lookup_mut(key)
  }

  pub fn size(&self) -> usize {
    ::std::mem::size_of::<TrieNode<V>>() +
      ::std::mem::size_of::<Option<KeyValue<Key, V>>>() * 2
      + self.children.iter().fold(0, |acc, c| acc + c.0.len() + c.1.size())
  }

  pub fn to_hashmap(&self) -> HashMap<Key, V> {
    let mut h = HashMap::new();

    self.to_hashmap_recursive(&mut h);

    h
  }

  pub fn to_hashmap_recursive(&self, h: &mut HashMap<Key, V>) {
    if let Some((ref key, ref value)) = self.key_value {
      h.insert(key.clone(), value.clone());
    }

    if let Some((ref key, ref value)) = self.wildcard {
      h.insert(key.clone(), value.clone());
    }

    for ref child in self.children.values() {
      child.to_hashmap_recursive(h);
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

    assert_eq!(root.domain_insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);
    root.print();

    assert_eq!(root.domain_lookup(&b"abce"[..]), Some(&((&b"abce"[..]).to_vec(), 2)));
    //assert!(false);
  }

  #[test]
  fn remove() {
    let mut root: TrieNode<u8> = TrieNode::root();
    println!("creating root:");
    root.print();

    println!("adding (abcd, 1)");
    assert_eq!(root.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    root.print();
    println!("adding (abce, 2)");
    assert_eq!(root.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
    root.print();
    println!("adding (abgh, 3)");
    assert_eq!(root.insert(Vec::from(&b"abgh"[..]), 3), InsertResult::Ok);
    root.print();

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
    let mut root1: TrieNode<u8> = TrieNode::root();

    println!("creating root1:");
    root1.print();
    println!("adding (abcd, 1)");
    assert_eq!(root1.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    root1.print();
    println!("adding (abce, 2)");
    assert_eq!(root1.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);
    root1.print();
    println!("adding (abc, 3)");
    assert_eq!(root1.insert(Vec::from(&b"abc"[..]), 3), InsertResult::Ok);

    println!("root1:");
    root1.print();

    let mut root2: TrieNode<u8> = TrieNode::root();

    assert_eq!(root2.insert(Vec::from(&b"abc"[..]), 3), InsertResult::Ok);
    assert_eq!(root2.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(root2.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);

    println!("root2:");
    root2.print();
    assert_eq!(root2.remove(&Vec::from(&b"abc"[..])), RemoveResult::Ok);

    println!("root2 after,remove:");
    root2.print();
    let mut expected: TrieNode<u8> = TrieNode::root();

    assert_eq!(expected.insert(Vec::from(&b"abcd"[..]), 1), InsertResult::Ok);
    assert_eq!(expected.insert(Vec::from(&b"abce"[..]), 2), InsertResult::Ok);

    println!("root2 after insert");
    root2.print();
    println!("expected");
    expected.print();
    assert_eq!(root2, expected);
  }

  #[test]
  fn domains() {
    let mut root: TrieNode<u8> = TrieNode::root();
    root.print();

    assert_eq!(root.domain_insert(Vec::from(&b"www.example.com"[..]), 1), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"test.example.com"[..]), 2), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"*.alldomains.org"[..]), 3), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"alldomains.org"[..]), 4), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"pouet.alldomains.org"[..]), 5), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"hello.com"[..]), 6), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"*.hello.com"[..]), 7), InsertResult::Ok);
    assert_eq!(root.domain_insert(Vec::from(&b"images./cdn[0-9]+/.hello.com"[..]), 8), InsertResult::Ok);
    root.print();
    assert_eq!(root.domain_insert(Vec::from(&b"/test[0-9]+/.www.hello.com"[..]), 9), InsertResult::Ok);
    root.print();

    assert_eq!(root.domain_lookup(&b"example.com"[..]), None);
    assert_eq!(root.domain_lookup(&b"blah.test.example.com"[..]), None);
    assert_eq!(root.domain_lookup(&b"www.example.com"[..]), Some(&((&b"www.example.com"[..]).to_vec(), 1)));
    assert_eq!(root.domain_lookup(&b"alldomains.org"[..]), Some(&((&b"alldomains.org"[..]).to_vec(), 4)));
    assert_eq!(root.domain_lookup(&b"test.hello.com"[..]), Some(&((&b"*.hello.com"[..]).to_vec(), 7)));
    assert_eq!(root.domain_lookup(&b"images.cdn10.hello.com"[..]), Some(&((&b"images./cdn[0-9]+/.hello.com"[..]).to_vec(), 8)));
    assert_eq!(root.domain_lookup(&b"test42.www.hello.com"[..]), Some(&((&b"/test[0-9]+/.www.hello.com"[..]).to_vec(), 9)));
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

  #[test]
  fn wildcard() {
    let mut root: TrieNode<u8> = TrieNode::root();
    root.print();
    root.domain_insert("*.clever-cloud.com".as_bytes().to_vec(), 2u8);
    root.domain_insert("services.clever-cloud.com".as_bytes().to_vec(), 0u8);
    root.domain_insert("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8);

    let res = root.domain_lookup(b"test.services.clever-cloud.com");
    println!("query result: {:?}", res);

    assert_eq!(
      root.domain_lookup(b"pgstudio.services.clever-cloud.com"),
      Some(&("*.services.clever-cloud.com".as_bytes().to_vec(), 1u8)));
  }

  fn hm_insert(h: std::collections::HashMap<String, u32>) -> bool {
    let mut root: TrieNode<u32> = TrieNode::root();

    for (k, v) in h.iter() {
      if k.is_empty() {
        continue;
      }

      if k.as_bytes()[0] == b'.' {
        continue;
      }

      if k.contains('/') {
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

      if k.as_bytes()[0] == b'.' {
        continue;
      }

      if k.contains('/') {
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
    fn qc_insert(h: std::collections::HashMap<String, u32>) -> bool {
      hm_insert(h)
    }
  }

  #[test]
  fn insert_disappearing_tree() {
    let h: std::collections::HashMap<String, u32> = [
      (String::from("\n\u{3}"), 0),
      (String::from("\n\u{0}"), 1),
      (String::from("\n"), 2)
    ].iter().cloned().collect();
    assert!(hm_insert(h));
  }

  #[test]
  fn size() {
    assert_size!(TrieNode<u32>, 128);
  }
}
