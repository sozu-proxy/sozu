use regex::bytes::Regex;
use std::str::from_utf8;
use sozu_command::proxy::{HttpFrontend,RulePosition, Route};

pub mod trie;
pub mod pattern_trie;

use self::pattern_trie::TrieNode;

pub struct Router {
  pre: Vec<(DomainRule, PathRule, Route)>,
  pub tree: TrieNode<Vec<(PathRule, Route)>>,
  post: Vec<(DomainRule, PathRule, Route)>,
}

impl Router {
  pub fn new() -> Router {
    Router {
      pre: Vec::new(),
      tree: TrieNode::root(),
      post: Vec::new(),
    }
  }

  pub fn lookup(&self, hostname: &[u8], path: &[u8]) -> Option<Route> {
    for (domain_rule, path_rule, app_id) in self.pre.iter() {
      if domain_rule.matches(hostname) && path_rule.matches(path) != PathRuleResult::None {
        return Some(app_id.clone());
      }
    }

    if let Some((_, path_rules)) = self.tree.lookup(hostname, true) {
      let mut prefix_length = 0;
      let mut res = None;

      for (rule, app_id) in path_rules.iter() {
        match rule.matches(path) {
          PathRuleResult::Regex => return Some(app_id.clone()),
          PathRuleResult::Prefix(sz) => if sz >= prefix_length {
            prefix_length = sz;
            res = Some(app_id);
          },
          PathRuleResult::None => {}
        }
      }

      if let Some(app_id) = res {
        return Some(app_id.clone());
      }
    }

    for (domain_rule, path_rule, app_id) in self.post.iter() {
      if domain_rule.matches(hostname) && path_rule.matches(path) != PathRuleResult::None {
        return Some(app_id.clone());
      }
    }

    None
  }

  pub fn add_http_front(&mut self, front: HttpFrontend) -> bool {
    match front.position {
      RulePosition::Pre => match (front.hostname.parse::<DomainRule>(), PathRule::from_config(front.path)) {
        (Ok(domain), Some(path)) => self.add_pre_rule(domain, path, front.route),
        _ => false,
      },
      RulePosition::Post => match (front.hostname.parse::<DomainRule>(), PathRule::from_config(front.path)) {
        (Ok(domain), Some(path)) => self.add_post_rule(domain, path, front.route),
        _ => false,
      },
      RulePosition::Tree => {
        match PathRule::from_config(front.path) {
          Some(path) => self.add_tree_rule(front.hostname.as_bytes(), path, front.route),
          _ => false,
        }
      }
    }
  }

  pub fn remove_http_front(&mut self, front: HttpFrontend) -> bool {
    match front.position {
      RulePosition::Pre => match (front.hostname.parse::<DomainRule>(), PathRule::from_config(front.path)) {
        (Ok(domain), Some(path)) => self.remove_pre_rule(domain, path),
        _ => false,
      },
      RulePosition::Post => match (front.hostname.parse::<DomainRule>(), PathRule::from_config(front.path)) {
        (Ok(domain), Some(path)) => self.remove_post_rule(domain, path),
        _ => false,
      },
      RulePosition::Tree => {
        match PathRule::from_config(front.path) {
          Some(path) => self.remove_tree_rule(front.hostname.as_bytes(), path, front.route),
          _ => false,
        }
      }
    }
  }

  pub fn add_tree_rule(&mut self, hostname: &[u8], path: PathRule, app_id: Route) -> bool {
    let hostname = match from_utf8(hostname) {
      Err(_) => return false,
      Ok(h) => h,
    };

    match ::idna::domain_to_ascii(hostname) {
      Ok(hostname) => {
        //FIXME: necessary ti build on stable rust (1.35), can be removed once 1.36 is there
        let mut empty = true;
        if let Some((_, ref mut paths)) = self.tree.domain_lookup_mut(hostname.as_bytes(), false) {
          empty = false;
          if paths.iter().find(|(p, _)| *p == path).is_none() {
            paths.push((path, app_id));
            return true;
          }
        }

        if empty {
          self.tree.domain_insert(hostname.into_bytes(), vec![(path, app_id)]);
          return true;
        }

        false
      },
      Err(_) => false
    }
  }

  pub fn remove_tree_rule(&mut self, hostname: &[u8], path: PathRule, _app_id: Route) -> bool {
    let hostname = match from_utf8(hostname) {
      Err(_) => return false,
      Ok(h) => h,
    };

    match ::idna::domain_to_ascii(hostname) {
      Ok(hostname) => {
        let should_delete = {
          let paths_opt = self.tree.domain_lookup_mut(hostname.as_bytes(), false);

          if let Some((_, paths)) = paths_opt {
            paths.retain(|(p, _)| *p != path);
          }

          paths_opt.as_ref().map(|(_, paths)| paths.is_empty()).unwrap_or(false)
        };

        if should_delete {
          self.tree.domain_remove(&hostname.into_bytes());
        }

        true
      },
      Err(_) => false,
    }
  }

  pub fn add_pre_rule(&mut self, domain: DomainRule, path: PathRule, app_id: Route) -> bool {
    if self.pre.iter().position(|(d, p, _)| *d == domain && *p == path).is_none() {
      self.pre.push((domain, path, app_id));
      true
    } else {
      false
    }
  }

  pub fn add_post_rule(&mut self, domain: DomainRule, path: PathRule, app_id: Route) -> bool {
    if self.post.iter().position(|(d, p, _)| *d == domain && *p == path).is_none() {
      self.post.push((domain, path, app_id));
      true
    } else {
      false
    }
  }

  pub fn remove_pre_rule(&mut self, domain: DomainRule, path: PathRule) -> bool {
    match self.pre.iter().position(|(d, p, _)| *d == domain && *p == path) {
      None => false,
      Some(index) => {
        self.pre.remove(index);
        true
      }
    }
  }

  pub fn remove_post_rule(&mut self, domain: DomainRule, path: PathRule) -> bool {
    match self.post.iter().position(|(d, p, _)| *d == domain && *p == path) {
      None => false,
      Some(index) => {
        self.post.remove(index);
        true
      }
    }
  }
}

#[derive(Clone,Debug)]
pub enum DomainRule {
  Any,
  Exact(String),
  Wildcard(String),
  Regex(Regex),
}

fn convert_regex_domain_rule(hostname: &str) -> Option<String> {
  let mut result = String::new();

  let s = hostname.as_bytes();
  let mut index = 0;
  loop {
    if s[index] == b'/' {
      let mut found = false;
      for i in index+1..s.len() {
        if s[i] == b'/' {
          match std::str::from_utf8(&s[index+1..i]) {
            Ok(r) => result.extend(r.chars()),
            Err(_) => return None,
          }
          index = i+1;
          found = true;
        }
      }

      if !found {
        return None;
      }
    } else {
      let start = index;
      for i in start..s.len()+1 {
        index = i;
        if i < s.len() && s[i] == b'.' {
          match std::str::from_utf8(&s[start..i]) {
            Ok(r) => result.extend(r.chars()),
            Err(_) => return None,
          }
          break;
        }
      }
      if index == s.len() {
        match std::str::from_utf8(&s[start..]) {
          Ok(r) => result.extend(r.chars()),
          Err(_) => return None,
        }
      }
    }

    if index == s.len() {

      return Some(result);
    } else {
      if s[index] == b'.' {
        result.extend("\\.".chars());
        index += 1;
      } else {
        return None;
      }
    }
  }
}

impl DomainRule {
  pub fn matches(&self, hostname: &[u8]) -> bool {
    match self {
      DomainRule::Any => true,
      DomainRule::Wildcard(s) => {
        let len_without_suffix = hostname.len() - s.len() + 1;
        hostname.ends_with(&s[1..].as_bytes()) &&
          !&hostname[..len_without_suffix].contains(&b'.')
      }
      DomainRule::Exact(s) => s.as_bytes() == hostname,
      DomainRule::Regex(r) => r.is_match(hostname),
    }
  }
}

impl std::cmp::PartialEq for DomainRule {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (DomainRule::Any, DomainRule::Any) => true,
      (DomainRule::Wildcard(s1), DomainRule::Wildcard(s2)) => s1 == s2,
      (DomainRule::Exact(s1), DomainRule::Exact(s2)) => s1 == s2,
      (DomainRule::Regex(r1), DomainRule::Regex(r2)) => r1.as_str() == r2.as_str(),
      _ => false,
    }
  }
}

impl std::str::FromStr for DomainRule {
  type Err = ();

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(if s == "*" {
      DomainRule::Any
    } else if s.contains('/') {
      match convert_regex_domain_rule(s) {
        Some(s) => match regex::bytes::Regex::new(&s) {
          Ok(r) => DomainRule::Regex(r),
          Err(_) => return Err(()),
        },
        None => return Err(())
      }
    } else if s.contains('*') {
      if s.chars().next() == Some('*') {
        match ::idna::domain_to_ascii(s) {
          Ok(r) => DomainRule::Wildcard(r),
          Err(_) => return Err(()),
        }
      } else {
        return Err(())
      }
    } else {
      match ::idna::domain_to_ascii(s) {
        Ok(r) => DomainRule::Exact(r),
        Err(_) => return Err(()),
      }
    })
  }
}

#[derive(Clone,Debug)]
pub enum PathRule {
  /// URI prefix, app
  Prefix(String),
  Regex(Regex),
}

#[derive(PartialEq)]
pub enum PathRuleResult {
  Regex,
  Prefix(usize),
  None,
}

impl PathRule {
  pub fn matches(&self, path: &[u8]) -> PathRuleResult {
    match self {
      PathRule::Prefix(s) => if path.starts_with(s.as_bytes()) {
        PathRuleResult::Prefix(s.len())
      } else {
        PathRuleResult::None
      },
      PathRule::Regex(r)  => if r.is_match(path) {
        PathRuleResult::Regex
      } else {
        PathRuleResult::None
      },
    }
  }

  pub fn from_config(rule: sozu_command::proxy::PathRule) -> Option<Self> {
    match rule {
      sozu_command::proxy::PathRule::Prefix(s) => Some(PathRule::Prefix(s)),
      sozu_command::proxy::PathRule::Regex(s)  => Regex::new(&s).ok().map(PathRule::Regex),
    }
  }
}

impl std::cmp::PartialEq for PathRule {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (PathRule::Prefix(s1), PathRule::Prefix(s2)) => s1 == s2,
      (PathRule::Regex(r1), PathRule::Regex(r2)) => r1.as_str() == r2.as_str(),
      _ => false,
    }
  }
}

/*
impl std::str::FromStr for PathRule {
  type Err = ();

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Ok(if s.chars().next() == Some('~') {
      match regex::bytes::Regex::new(&s[1..]) {
        Ok(r) => PathRule::Regex(r),
        Err(_) => return Err(()),
      }
    } else {
      PathRule::Prefix(s.to_string())
    })
  }
}
*/

#[cfg(test)]
mod tests {
  use super::*;
  use sozu_command::proxy::Route;

  #[test]
  fn convert_regex() {
    assert_eq!(convert_regex_domain_rule("www.example.com").unwrap().as_str(), "www\\.example\\.com");
    assert_eq!(convert_regex_domain_rule("*.example.com").unwrap().as_str(), "*\\.example\\.com");
    assert_eq!(convert_regex_domain_rule("test.*.example.com").unwrap().as_str(), "test\\.*\\.example\\.com");
    assert_eq!(convert_regex_domain_rule("css./cdn[a-z0-9]+/.example.com").unwrap().as_str(), "css\\.cdn[a-z0-9]+\\.example\\.com");

    assert_eq!(convert_regex_domain_rule("css./cdn[a-z0-9]+.example.com"), None);
    assert_eq!(convert_regex_domain_rule("css./cdn[a-z0-9]+/a.example.com"), None);
  }

  #[test]
  fn parse_domain_rule() {
    assert_eq!("*".parse::<DomainRule>().unwrap(), DomainRule::Any);
    assert_eq!("www.example.com".parse::<DomainRule>().unwrap(), DomainRule::Exact("www.example.com".to_string()));
    assert_eq!("*.example.com".parse::<DomainRule>().unwrap(), DomainRule::Wildcard("*.example.com".to_string()));
    assert_eq!("test.*.example.com".parse::<DomainRule>(), Err(()));
    assert_eq!("/cdn[0-9]+/.example.com".parse::<DomainRule>().unwrap(), DomainRule::Regex(Regex::new("cdn[0-9]+\\.example\\.com").unwrap()));
  }

  #[test]
  fn match_domain_rule() {
    assert!(DomainRule::Any.matches("www.example.com".as_bytes()));
    assert!(DomainRule::Exact("www.example.com".to_string()).matches("www.example.com".as_bytes()));
    assert!(DomainRule::Wildcard("*.example.com".to_string()).matches("www.example.com".as_bytes()));
    assert!(!DomainRule::Wildcard("*.example.com".to_string()).matches("test.www.example.com".as_bytes()));
    assert!("/cdn[0-9]+/.example.com".parse::<DomainRule>().unwrap().matches("cdn1.example.com".as_bytes()));
    assert!(! "/cdn[0-9]+/.example.com".parse::<DomainRule>().unwrap().matches("www.example.com".as_bytes()));
    assert!(! "/cdn[0-9]+/.example.com".parse::<DomainRule>().unwrap().matches("cdn10.exampleAcom".as_bytes()));
  }

  #[test]
  fn match_path_rule() {
    assert!(PathRule::Prefix("".to_string()).matches("/".as_bytes()) != PathRuleResult::None);
    assert!(PathRule::Prefix("".to_string()).matches("/hello".as_bytes()) != PathRuleResult::None);
    assert!(PathRule::Prefix("/hello".to_string()).matches("/hello".as_bytes()) != PathRuleResult::None);
    assert!(PathRule::Prefix("/hello".to_string()).matches("/hello/world".as_bytes()) != PathRuleResult::None);
    assert!(PathRule::Prefix("/hello".to_string()).matches("/".as_bytes()) == PathRuleResult::None);
  }

  #[test]
  fn match_router() {
    let mut router = Router::new();

    assert!(router.add_pre_rule("*".parse::<DomainRule>().unwrap(), PathRule::Prefix("/.well-known/acme-challenge".to_string()), Route::AppId("acme".to_string())));
    assert!(router.add_tree_rule("www.example.com".as_bytes(), PathRule::Prefix("/".to_string()), Route::AppId("example".to_string())));
    assert!(router.add_tree_rule("*.test.example.com".as_bytes(), PathRule::Regex(Regex::new("/hello[A-Z]+/").unwrap()), Route::AppId("examplewildcard".to_string())));
    assert!(router.add_tree_rule("/test[0-9]/.example.com".as_bytes(), PathRule::Prefix("/".to_string()), Route::AppId("exampleregex".to_string())));

    assert_eq!(router.lookup("www.example.com".as_bytes(), "/helloA".as_bytes()), Some(Route::AppId("example".to_string())));
    assert_eq!(router.lookup("www.example.com".as_bytes(), "/.well-known/acme-challenge".as_bytes()), Some(Route::AppId("acme".to_string())));
    assert_eq!(router.lookup("www.test.example.com".as_bytes(), "/".as_bytes()), None);
    assert_eq!(router.lookup("www.test.example.com".as_bytes(), "/helloAB/".as_bytes()), Some(Route::AppId("examplewildcard".to_string())));
    assert_eq!(router.lookup("test1.example.com".as_bytes(), "/helloAB/".as_bytes()), Some(Route::AppId("exampleregex".to_string())));
  }
}
