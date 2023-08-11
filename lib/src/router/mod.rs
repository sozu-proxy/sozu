pub mod pattern_trie;
pub mod trie;

use regex::bytes::Regex;
use std::str::from_utf8;
use time::Instant;

use crate::protocol::http::parser::Method;
use sozu_command::{
    proto::command::{PathRule as CommandPathRule, PathRuleKind, RulePosition},
    response::HttpFrontend,
    state::ClusterId,
};

use self::pattern_trie::TrieNode;

#[derive(thiserror::Error, Debug)]
pub enum RouterError {
    #[error("Could not parse rule from frontend path {0:?}")]
    InvalidPathRule(String),
    #[error("parsing hostname {hostname} failed")]
    InvalidDomain { hostname: String },
    #[error("Could not add route {0}")]
    AddRoute(String),
    #[error("Could not remove route {0}")]
    RemoveRoute(String),
}

pub struct Router {
    pre: Vec<(DomainRule, PathRule, MethodRule, Route)>,
    pub tree: TrieNode<Vec<(PathRule, MethodRule, Route)>>,
    post: Vec<(DomainRule, PathRule, MethodRule, Route)>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    pub fn new() -> Router {
        Router {
            pre: Vec::new(),
            tree: TrieNode::root(),
            post: Vec::new(),
        }
    }

    pub fn lookup(&self, hostname: &[u8], path: &[u8], method: &Method) -> Option<Route> {
        for (domain_rule, path_rule, method_rule, cluster_id) in &self.pre {
            if domain_rule.matches(hostname)
                && path_rule.matches(path) != PathRuleResult::None
                && method_rule.matches(method) != MethodRuleResult::None
            {
                return Some(cluster_id.clone());
            }
        }

        if let Some((_, path_rules)) = self.tree.lookup(hostname, true) {
            let mut prefix_length = 0;
            let mut res = None;

            for (rule, method_rule, cluster_id) in path_rules {
                match rule.matches(path) {
                    PathRuleResult::Regex | PathRuleResult::Equals => {
                        match method_rule.matches(method) {
                            MethodRuleResult::Equals => return Some(cluster_id.clone()),
                            MethodRuleResult::All => {
                                prefix_length = path.len();
                                res = Some(cluster_id);
                            }
                            MethodRuleResult::None => {}
                        }
                    }
                    PathRuleResult::Prefix(size) => {
                        if size >= prefix_length {
                            match method_rule.matches(method) {
                                // FIXME: the rule order will be important here
                                MethodRuleResult::Equals => {
                                    prefix_length = size;
                                    res = Some(cluster_id);
                                }
                                MethodRuleResult::All => {
                                    prefix_length = size;
                                    res = Some(cluster_id);
                                }
                                MethodRuleResult::None => {}
                            }
                        }
                    }
                    PathRuleResult::None => {}
                }
            }

            if let Some(cluster_id) = res {
                return Some(cluster_id.clone());
            }
        }

        for (domain_rule, path_rule, method_rule, cluster_id) in self.post.iter() {
            if domain_rule.matches(hostname)
                && path_rule.matches(path) != PathRuleResult::None
                && method_rule.matches(method) != MethodRuleResult::None
            {
                return Some(cluster_id.clone());
            }
        }

        None
    }

    pub fn add_http_front(&mut self, front: &HttpFrontend) -> Result<(), RouterError> {
        let path_rule = PathRule::from_config(front.path.clone())
            .ok_or(RouterError::InvalidPathRule(front.path.to_string()))?;

        let method_rule = MethodRule::new(front.method.clone());

        let route = match &front.cluster_id {
            Some(cluster_id) => Route::Cluster {
                id: cluster_id.clone(),
                h2: front.h2,
            },
            None => Route::Deny,
        };

        let success = match front.position {
            RulePosition::Pre => {
                let domain = front.hostname.parse::<DomainRule>().map_err(|_| {
                    RouterError::InvalidDomain {
                        hostname: front.hostname.clone(),
                    }
                })?;

                self.add_pre_rule(&domain, &path_rule, &method_rule, &route)
            }
            RulePosition::Post => {
                let domain = front.hostname.parse::<DomainRule>().map_err(|_| {
                    RouterError::InvalidDomain {
                        hostname: front.hostname.clone(),
                    }
                })?;

                self.add_post_rule(&domain, &path_rule, &method_rule, &route)
            }
            RulePosition::Tree => {
                self.add_tree_rule(front.hostname.as_bytes(), &path_rule, &method_rule, &route)
            }
        };
        if !success {
            return Err(RouterError::AddRoute(format!("{front:?}")));
        }
        Ok(())
    }

    pub fn remove_http_front(&mut self, front: &HttpFrontend) -> Result<(), RouterError> {
        let path_rule = PathRule::from_config(front.path.clone())
            .ok_or(RouterError::InvalidPathRule(front.path.to_string()))?;

        let method_rule = MethodRule::new(front.method.clone());

        let remove_success = match front.position {
            RulePosition::Pre => {
                let domain = front.hostname.parse::<DomainRule>().map_err(|_| {
                    RouterError::InvalidDomain {
                        hostname: front.hostname.clone(),
                    }
                })?;

                self.remove_pre_rule(&domain, &path_rule, &method_rule)
            }
            RulePosition::Post => {
                let domain = front.hostname.parse::<DomainRule>().map_err(|_| {
                    RouterError::InvalidDomain {
                        hostname: front.hostname.clone(),
                    }
                })?;

                self.remove_post_rule(&domain, &path_rule, &method_rule)
            }
            RulePosition::Tree => {
                self.remove_tree_rule(front.hostname.as_bytes(), &path_rule, &method_rule)
            }
        };
        if !remove_success {
            return Err(RouterError::RemoveRoute(format!("{front:?}")));
        }
        Ok(())
    }

    pub fn add_tree_rule(
        &mut self,
        hostname: &[u8],
        path: &PathRule,
        method: &MethodRule,
        cluster: &Route,
    ) -> bool {
        let hostname = match from_utf8(hostname) {
            Err(_) => return false,
            Ok(h) => h,
        };

        match ::idna::domain_to_ascii(hostname) {
            Ok(hostname) => {
                //FIXME: necessary ti build on stable rust (1.35), can be removed once 1.36 is there
                let mut empty = true;
                if let Some((_, ref mut paths)) =
                    self.tree.domain_lookup_mut(hostname.as_bytes(), false)
                {
                    empty = false;
                    if !paths.iter().any(|(p, m, _)| p == path && m == method) {
                        paths.push((path.to_owned(), method.to_owned(), cluster.to_owned()));
                        return true;
                    }
                }

                if empty {
                    self.tree.domain_insert(
                        hostname.into_bytes(),
                        vec![(path.to_owned(), method.to_owned(), cluster.to_owned())],
                    );
                    return true;
                }

                false
            }
            Err(_) => false,
        }
    }

    pub fn remove_tree_rule(
        &mut self,
        hostname: &[u8],
        path: &PathRule,
        method: &MethodRule,
        // _cluster: &Route,
    ) -> bool {
        let hostname = match from_utf8(hostname) {
            Err(_) => return false,
            Ok(h) => h,
        };

        match ::idna::domain_to_ascii(hostname) {
            Ok(hostname) => {
                let should_delete = {
                    let paths_opt = self.tree.domain_lookup_mut(hostname.as_bytes(), false);

                    if let Some((_, paths)) = paths_opt {
                        paths.retain(|(p, m, _)| p != path || m != method);
                    }

                    paths_opt
                        .as_ref()
                        .map(|(_, paths)| paths.is_empty())
                        .unwrap_or(false)
                };

                if should_delete {
                    self.tree.domain_remove(&hostname.into_bytes());
                }

                true
            }
            Err(_) => false,
        }
    }

    pub fn add_pre_rule(
        &mut self,
        domain: &DomainRule,
        path: &PathRule,
        method: &MethodRule,
        cluster_id: &Route,
    ) -> bool {
        if !self
            .pre
            .iter()
            .any(|(d, p, m, _)| d == domain && p == path && m == method)
        {
            self.pre.push((
                domain.to_owned(),
                path.to_owned(),
                method.to_owned(),
                cluster_id.to_owned(),
            ));
            true
        } else {
            false
        }
    }

    pub fn add_post_rule(
        &mut self,
        domain: &DomainRule,
        path: &PathRule,
        method: &MethodRule,
        cluster_id: &Route,
    ) -> bool {
        if !self
            .post
            .iter()
            .any(|(d, p, m, _)| d == domain && p == path && m == method)
        {
            self.post.push((
                domain.to_owned(),
                path.to_owned(),
                method.to_owned(),
                cluster_id.to_owned(),
            ));
            true
        } else {
            false
        }
    }

    pub fn remove_pre_rule(
        &mut self,
        domain: &DomainRule,
        path: &PathRule,
        method: &MethodRule,
    ) -> bool {
        match self
            .pre
            .iter()
            .position(|(d, p, m, _)| d == domain && p == path && m == method)
        {
            None => false,
            Some(index) => {
                self.pre.remove(index);
                true
            }
        }
    }

    pub fn remove_post_rule(
        &mut self,
        domain: &DomainRule,
        path: &PathRule,
        method: &MethodRule,
    ) -> bool {
        match self
            .post
            .iter()
            .position(|(d, p, m, _)| d == domain && p == path && m == method)
        {
            None => false,
            Some(index) => {
                self.post.remove(index);
                true
            }
        }
    }
}

#[derive(Clone, Debug)]
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
            for i in index + 1..s.len() {
                if s[i] == b'/' {
                    match std::str::from_utf8(&s[index + 1..i]) {
                        Ok(r) => result.push_str(r),
                        Err(_) => return None,
                    }
                    index = i + 1;
                    found = true;
                }
            }

            if !found {
                return None;
            }
        } else {
            let start = index;
            for i in start..s.len() + 1 {
                index = i;
                if i < s.len() && s[i] == b'.' {
                    match std::str::from_utf8(&s[start..i]) {
                        Ok(r) => result.push_str(r),
                        Err(_) => return None,
                    }
                    break;
                }
            }
            if index == s.len() {
                match std::str::from_utf8(&s[start..]) {
                    Ok(r) => result.push_str(r),
                    Err(_) => return None,
                }
            }
        }

        if index == s.len() {
            return Some(result);
        } else if s[index] == b'.' {
            result.push_str("\\.");
            index += 1;
        } else {
            return None;
        }
    }
}

impl DomainRule {
    pub fn matches(&self, hostname: &[u8]) -> bool {
        match self {
            DomainRule::Any => true,
            DomainRule::Wildcard(s) => {
                let len_without_suffix = hostname.len() - s.len() + 1;
                hostname.ends_with(s[1..].as_bytes())
                    && !&hostname[..len_without_suffix].contains(&b'.')
            }
            DomainRule::Exact(s) => s.as_bytes() == hostname,
            DomainRule::Regex(r) => {
                let start = Instant::now();
                let is_a_match = r.is_match(hostname);
                let now = Instant::now();
                time!("regex_matching_time", (now - start).whole_milliseconds());
                is_a_match
            }
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
                None => return Err(()),
            }
        } else if s.contains('*') {
            if s.starts_with('*') {
                match ::idna::domain_to_ascii(s) {
                    Ok(r) => DomainRule::Wildcard(r),
                    Err(_) => return Err(()),
                }
            } else {
                return Err(());
            }
        } else {
            match ::idna::domain_to_ascii(s) {
                Ok(r) => DomainRule::Exact(r),
                Err(_) => return Err(()),
            }
        })
    }
}

#[derive(Clone, Debug)]
pub enum PathRule {
    Prefix(String),
    Regex(Regex),
    Equals(String),
}

#[derive(PartialEq, Eq)]
pub enum PathRuleResult {
    Regex,
    Prefix(usize),
    Equals,
    None,
}

impl PathRule {
    pub fn matches(&self, path: &[u8]) -> PathRuleResult {
        match self {
            PathRule::Prefix(s) => {
                if path.starts_with(s.as_bytes()) {
                    PathRuleResult::Prefix(s.len())
                } else {
                    PathRuleResult::None
                }
            }
            PathRule::Regex(r) => {
                let start = Instant::now();
                let is_a_match = r.is_match(path);
                let now = Instant::now();
                time!("regex_matching_time", (now - start).whole_milliseconds());

                if is_a_match {
                    PathRuleResult::Regex
                } else {
                    PathRuleResult::None
                }
            }
            PathRule::Equals(s) => {
                if path == s.as_bytes() {
                    PathRuleResult::Equals
                } else {
                    PathRuleResult::None
                }
            }
        }
    }

    pub fn from_config(rule: CommandPathRule) -> Option<Self> {
        match PathRuleKind::from_i32(rule.kind) {
            Some(PathRuleKind::Prefix) => Some(PathRule::Prefix(rule.value)),
            Some(PathRuleKind::Regex) => Regex::new(&rule.value).ok().map(PathRule::Regex),
            Some(PathRuleKind::Equals) => Some(PathRule::Equals(rule.value)),
            _ => None,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodRule {
    pub inner: Option<Method>,
}

#[derive(PartialEq, Eq)]
pub enum MethodRuleResult {
    All,
    Equals,
    None,
}

impl MethodRule {
    pub fn new(method: Option<String>) -> Self {
        MethodRule {
            inner: method.map(|s| Method::new(s.as_bytes())),
        }
    }

    pub fn matches(&self, method: &Method) -> MethodRuleResult {
        match self.inner {
            None => MethodRuleResult::All,
            Some(ref m) => {
                if method == m {
                    MethodRuleResult::Equals
                } else {
                    MethodRuleResult::None
                }
            }
        }
    }
}

/// The cluster to which the traffic will be redirected
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Route {
    /// send a 401 default answer
    Deny,
    /// the cluster to which the frontend belongs
    Cluster { id: ClusterId, h2: bool },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_regex() {
        assert_eq!(
            convert_regex_domain_rule("www.example.com")
                .unwrap()
                .as_str(),
            "www\\.example\\.com"
        );
        assert_eq!(
            convert_regex_domain_rule("*.example.com").unwrap().as_str(),
            "*\\.example\\.com"
        );
        assert_eq!(
            convert_regex_domain_rule("test.*.example.com")
                .unwrap()
                .as_str(),
            "test\\.*\\.example\\.com"
        );
        assert_eq!(
            convert_regex_domain_rule("css./cdn[a-z0-9]+/.example.com")
                .unwrap()
                .as_str(),
            "css\\.cdn[a-z0-9]+\\.example\\.com"
        );

        assert_eq!(
            convert_regex_domain_rule("css./cdn[a-z0-9]+.example.com"),
            None
        );
        assert_eq!(
            convert_regex_domain_rule("css./cdn[a-z0-9]+/a.example.com"),
            None
        );
    }

    #[test]
    fn parse_domain_rule() {
        assert_eq!("*".parse::<DomainRule>().unwrap(), DomainRule::Any);
        assert_eq!(
            "www.example.com".parse::<DomainRule>().unwrap(),
            DomainRule::Exact("www.example.com".to_string())
        );
        assert_eq!(
            "*.example.com".parse::<DomainRule>().unwrap(),
            DomainRule::Wildcard("*.example.com".to_string())
        );
        assert_eq!("test.*.example.com".parse::<DomainRule>(), Err(()));
        assert_eq!(
            "/cdn[0-9]+/.example.com".parse::<DomainRule>().unwrap(),
            DomainRule::Regex(Regex::new("cdn[0-9]+\\.example\\.com").unwrap())
        );
    }

    #[test]
    fn match_domain_rule() {
        assert!(DomainRule::Any.matches("www.example.com".as_bytes()));
        assert!(
            DomainRule::Exact("www.example.com".to_string()).matches("www.example.com".as_bytes())
        );
        assert!(
            DomainRule::Wildcard("*.example.com".to_string()).matches("www.example.com".as_bytes())
        );
        assert!(!DomainRule::Wildcard("*.example.com".to_string())
            .matches("test.www.example.com".as_bytes()));
        assert!("/cdn[0-9]+/.example.com"
            .parse::<DomainRule>()
            .unwrap()
            .matches("cdn1.example.com".as_bytes()));
        assert!(!"/cdn[0-9]+/.example.com"
            .parse::<DomainRule>()
            .unwrap()
            .matches("www.example.com".as_bytes()));
        assert!(!"/cdn[0-9]+/.example.com"
            .parse::<DomainRule>()
            .unwrap()
            .matches("cdn10.exampleAcom".as_bytes()));
    }

    #[test]
    fn match_path_rule() {
        assert!(PathRule::Prefix("".to_string()).matches("/".as_bytes()) != PathRuleResult::None);
        assert!(
            PathRule::Prefix("".to_string()).matches("/hello".as_bytes()) != PathRuleResult::None
        );
        assert!(
            PathRule::Prefix("/hello".to_string()).matches("/hello".as_bytes())
                != PathRuleResult::None
        );
        assert!(
            PathRule::Prefix("/hello".to_string()).matches("/hello/world".as_bytes())
                != PathRuleResult::None
        );
        assert!(
            PathRule::Prefix("/hello".to_string()).matches("/".as_bytes()) == PathRuleResult::None
        );
    }

    #[test]
    fn match_router() {
        let mut router = Router::new();

        assert!(router.add_pre_rule(
            &"*".parse::<DomainRule>().unwrap(),
            &PathRule::Prefix("/.well-known/acme-challenge".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::Cluster {
                id: "acme".to_string(),
                h2: false
            }
        ));
        assert!(router.add_tree_rule(
            "www.example.com".as_bytes(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::Cluster {
                id: "example".to_string(),
                h2: false
            }
        ));
        assert!(router.add_tree_rule(
            "*.test.example.com".as_bytes(),
            &PathRule::Regex(Regex::new("/hello[A-Z]+/").unwrap()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::Cluster {
                id: "examplewildcard".to_string(),
                h2: false
            }
        ));
        assert!(router.add_tree_rule(
            "/test[0-9]/.example.com".as_bytes(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::Cluster {
                id: "exampleregex".to_string(),
                h2: false
            }
        ));

        assert_eq!(
            router.lookup(
                "www.example.com".as_bytes(),
                "/helloA".as_bytes(),
                &Method::new(&b"GET"[..])
            ),
            Some(Route::Cluster {
                id: "example".to_string(),
                h2: false
            })
        );
        assert_eq!(
            router.lookup(
                "www.example.com".as_bytes(),
                "/.well-known/acme-challenge".as_bytes(),
                &Method::new(&b"GET"[..])
            ),
            Some(Route::Cluster {
                id: "acme".to_string(),
                h2: false
            })
        );
        assert_eq!(
            router.lookup(
                "www.test.example.com".as_bytes(),
                "/".as_bytes(),
                &Method::new(&b"GET"[..])
            ),
            None
        );
        assert_eq!(
            router.lookup(
                "www.test.example.com".as_bytes(),
                "/helloAB/".as_bytes(),
                &Method::new(&b"GET"[..])
            ),
            Some(Route::Cluster {
                id: "examplewildcard".to_string(),
                h2: false
            })
        );
        assert_eq!(
            router.lookup(
                "test1.example.com".as_bytes(),
                "/helloAB/".as_bytes(),
                &Method::new(&b"GET"[..])
            ),
            Some(Route::Cluster {
                id: "exampleregex".to_string(),
                h2: false
            })
        );
    }
}
