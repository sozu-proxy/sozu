pub mod pattern_trie;

use std::{
    fmt::{Debug, Write},
    rc::Rc,
    str::{from_utf8, from_utf8_unchecked},
    time::Instant,
};

use nom::AsChar;
use pattern_trie::{TrieMatches, TrieSubMatch};
use regex::bytes::Regex;

use sozu_command::{
    logging::CachedTags,
    proto::command::{
        PathRule as CommandPathRule, PathRuleKind, Position, RedirectPolicy, RedirectScheme,
        RulePosition,
    },
    response::HttpFrontend,
    state::ClusterId,
};

use crate::{protocol::http::parser::Method, router::pattern_trie::TrieNode};

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum RouterError {
    #[error("Could not parse rule from frontend path {0:?}")]
    InvalidPathRule(String),
    #[error("Could not parse hostname {0:?}")]
    InvalidDomain(String),
    #[error("Could not parse host rewrite {0:?}")]
    InvalidHostRewrite(String),
    #[error("Could not parse path rewrite {0:?}")]
    InvalidPathRewrite(String),
    #[error("Could not add route {0}")]
    AddRoute(String),
    #[error("Could not remove route {0}")]
    RemoveRoute(String),
    #[error("no route for {method} {host} {path}")]
    RouteNotFound {
        host: String,
        path: String,
        method: Method,
    },
}

pub struct Router {
    pre: Vec<(DomainRule, PathRule, MethodRule, Frontend)>,
    pub tree: TrieNode<Vec<(PathRule, MethodRule, Frontend)>>,
    post: Vec<(DomainRule, PathRule, MethodRule, Frontend)>,
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

    pub fn lookup<'a>(
        &self,
        hostname: &'a str,
        path: &'a str,
        method: &'a Method,
    ) -> Result<RouteResult, RouterError> {
        let hostname_b = hostname.as_bytes();
        let path_b = path.as_bytes();
        for (domain_rule, path_rule, method_rule, route) in &self.pre {
            if domain_rule.matches(hostname_b)
                && path_rule.matches(path_b) != PathRuleResult::None
                && method_rule.matches(method) != MethodRuleResult::None
            {
                return Ok(RouteResult::new_no_trie(
                    hostname_b,
                    domain_rule,
                    path_b,
                    path_rule,
                    route,
                ));
            }
        }

        let trie_path = Vec::with_capacity(16);
        if let Some(((_, rules), trie_path)) = self.tree.lookup(hostname_b, true, trie_path) {
            let mut prefix_length = 0;
            let mut frontend = None;

            for (path_rule, method_rule, route) in rules {
                match path_rule.matches(path_b) {
                    PathRuleResult::Regex | PathRuleResult::Equals => {
                        match method_rule.matches(method) {
                            MethodRuleResult::Equals => {
                                return Ok(RouteResult::new_with_trie(
                                    hostname_b, trie_path, path_b, path_rule, route,
                                ))
                            }
                            MethodRuleResult::All => {
                                prefix_length = path_b.len();
                                frontend = Some((path_rule, route));
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
                                    frontend = Some((path_rule, route));
                                }
                                MethodRuleResult::All => {
                                    prefix_length = size;
                                    frontend = Some((path_rule, route));
                                }
                                MethodRuleResult::None => {}
                            }
                        }
                    }
                    PathRuleResult::None => {}
                }
            }

            if let Some((path_rule, route)) = frontend {
                return Ok(RouteResult::new_with_trie(
                    hostname_b, trie_path, path_b, path_rule, route,
                ));
            }
        }

        for (domain_rule, path_rule, method_rule, route) in self.post.iter() {
            if domain_rule.matches(hostname_b)
                && path_rule.matches(path_b) != PathRuleResult::None
                && method_rule.matches(method) != MethodRuleResult::None
            {
                return Ok(RouteResult::new_no_trie(
                    hostname_b,
                    domain_rule,
                    path_b,
                    path_rule,
                    route,
                ));
            }
        }

        Err(RouterError::RouteNotFound {
            host: hostname.to_owned(),
            path: path.to_owned(),
            method: method.to_owned(),
        })
    }

    pub fn add_http_front(&mut self, front: &HttpFrontend) -> Result<(), RouterError> {
        let domain_rule = front
            .hostname
            .parse::<DomainRule>()
            .map_err(|_| RouterError::InvalidDomain(front.hostname.clone()))?;

        let path_rule = PathRule::from_config(front.path.clone())
            .ok_or(RouterError::InvalidPathRule(front.path.to_string()))?;

        let method_rule = MethodRule::new(front.method.clone());

        let route = Frontend::new(&domain_rule, &path_rule, front)?;

        let success = match front.position {
            RulePosition::Pre => self.add_pre_rule(&domain_rule, &path_rule, &method_rule, &route),
            RulePosition::Post => {
                self.add_post_rule(&domain_rule, &path_rule, &method_rule, &route)
            }
            RulePosition::Tree => {
                self.add_tree_rule(front.hostname.as_bytes(), &path_rule, &method_rule, &route)
            }
        };
        if !success {
            return Err(RouterError::AddRoute(format!("{:?}", front)));
        }
        Ok(())
    }

    pub fn remove_http_front(&mut self, front: &HttpFrontend) -> Result<(), RouterError> {
        let path_rule = PathRule::from_config(front.path.clone())
            .ok_or(RouterError::InvalidPathRule(front.path.to_string()))?;

        let method_rule = MethodRule::new(front.method.clone());

        let remove_success = match front.position {
            RulePosition::Pre => {
                let domain_rule = front
                    .hostname
                    .parse::<DomainRule>()
                    .map_err(|_| RouterError::InvalidDomain(front.hostname.clone()))?;

                self.remove_pre_rule(&domain_rule, &path_rule, &method_rule)
            }
            RulePosition::Post => {
                let domain_rule = front
                    .hostname
                    .parse::<DomainRule>()
                    .map_err(|_| RouterError::InvalidDomain(front.hostname.clone()))?;

                self.remove_post_rule(&domain_rule, &path_rule, &method_rule)
            }
            RulePosition::Tree => {
                self.remove_tree_rule(front.hostname.as_bytes(), &path_rule, &method_rule)
            }
        };
        if !remove_success {
            return Err(RouterError::RemoveRoute(format!("{:?}", front)));
        }
        Ok(())
    }

    pub fn add_tree_rule(
        &mut self,
        hostname: &[u8],
        path: &PathRule,
        method: &MethodRule,
        cluster: &Frontend,
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
        cluster_id: &Frontend,
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
        cluster_id: &Frontend,
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
    Equals(String),
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
                    break;
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
            DomainRule::Equals(s) => s.as_bytes() == hostname,
            DomainRule::Regex(r) => {
                let start = Instant::now();
                let is_a_match = r.is_match(hostname);
                let now = Instant::now();
                time!("regex_matching_time", (now - start).as_millis());
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
            (DomainRule::Equals(s1), DomainRule::Equals(s2)) => s1 == s2,
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
                Ok(r) => DomainRule::Equals(r),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRuleResult {
    Regex,
    Prefix(usize),
    Equals,
    None,
}

impl PathRule {
    pub fn matches(&self, path: &[u8]) -> PathRuleResult {
        match self {
            PathRule::Prefix(prefix) => {
                if path.starts_with(prefix.as_bytes()) {
                    PathRuleResult::Prefix(prefix.len())
                } else {
                    PathRuleResult::None
                }
            }
            PathRule::Regex(regex) => {
                let start = Instant::now();
                let is_a_match = regex.is_match(path);
                let now = Instant::now();
                time!("regex_matching_time", (now - start).as_millis());

                if is_a_match {
                    PathRuleResult::Regex
                } else {
                    PathRuleResult::None
                }
            }
            PathRule::Equals(pattern) => {
                if path == pattern.as_bytes() {
                    PathRuleResult::Equals
                } else {
                    PathRuleResult::None
                }
            }
        }
    }

    pub fn from_config(rule: CommandPathRule) -> Option<Self> {
        match PathRuleKind::try_from(rule.kind) {
            Ok(PathRuleKind::Prefix) => Some(PathRule::Prefix(rule.value)),
            Ok(PathRuleKind::Regex) => Regex::new(&rule.value).ok().map(PathRule::Regex),
            Ok(PathRuleKind::Equals) => Some(PathRule::Equals(rule.value)),
            Err(_) => None,
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

#[derive(Debug, Clone)]
enum RewritePart {
    String(String),
    Host(usize),
    Path(usize),
}
impl RewritePart {
    pub fn string(s: &str) -> Self {
        Self::String(String::from(s))
    }
    pub fn bytes(b: &[u8]) -> Self {
        Self::String(unsafe { String::from_utf8_unchecked(b.to_vec()) })
    }
}

#[derive(Debug, Clone)]
pub struct RewriteParts(Vec<RewritePart>);
impl RewriteParts {
    pub fn new(
        pattern: &str,
        index_max_host: usize,
        index_max_path: usize,
        used_index_host: &mut usize,
        used_index_path: &mut usize,
    ) -> Option<Self> {
        let mut result = Vec::new();
        let mut i = 0;
        let pattern = pattern.as_bytes();
        while i < pattern.len() {
            if pattern[i] == b'$' {
                let is_host = if pattern[i..].starts_with(b"$HOST[") {
                    i += 6;
                    true
                } else if pattern[i..].starts_with(b"$PATH[") {
                    i += 6;
                    false
                } else {
                    return None;
                };
                let mut index = 0;
                while i < pattern.len() && pattern[i].is_dec_digit() {
                    index = index * 10 + (pattern[i] - b'0') as usize;
                    i += 1;
                }
                if i >= pattern.len() || pattern[i] != b']' {
                    return None;
                }
                if is_host {
                    if index >= index_max_host {
                        return None;
                    }
                    if index >= *used_index_host {
                        *used_index_host = index + 1;
                    }
                    result.push(RewritePart::Host(index));
                } else {
                    if index >= index_max_path {
                        return None;
                    }
                    if index >= *used_index_path {
                        *used_index_path = index + 1;
                    }
                    result.push(RewritePart::Path(index));
                }
                i += 1;
            } else {
                let start = i;
                while i < pattern.len() && pattern[i] != b'$' {
                    i += 1;
                }
                result.push(RewritePart::bytes(&pattern[start..i]));
            }
        }
        Some(Self(result))
    }
    pub fn run(&self, host_captures: &[&str], path_captures: &[&str]) -> String {
        let mut cap = 0;
        for part in &self.0 {
            cap += match part {
                RewritePart::String(s) => s.len(),
                RewritePart::Host(i) => unsafe { host_captures.get_unchecked(*i).len() },
                RewritePart::Path(i) => unsafe { path_captures.get_unchecked(*i).len() },
            };
        }
        let mut result = String::with_capacity(cap);
        for part in &self.0 {
            let _ = match part {
                RewritePart::String(s) => result.write_str(s),
                RewritePart::Host(i) => {
                    result.write_str(unsafe { host_captures.get_unchecked(*i) })
                }
                RewritePart::Path(i) => {
                    result.write_str(unsafe { path_captures.get_unchecked(*i) })
                }
            };
        }
        result
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HeaderEdit {
    pub key: Rc<[u8]>,
    pub val: Rc<[u8]>,
}

impl Debug for HeaderEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "({:?}, {:?})",
            String::from_utf8_lossy(&self.key),
            String::from_utf8_lossy(&self.val)
        ))
    }
}

/// What to do with the traffic
/// TODO: tags should be moved here
#[derive(Debug, Clone)]
pub struct Frontend {
    cluster_id: Option<ClusterId>,
    required_auth: bool,
    redirect: RedirectPolicy,
    redirect_scheme: RedirectScheme,
    redirect_template: Option<String>,
    capture_cap_host: usize,
    capture_cap_path: usize,
    rewrite_host: Option<RewriteParts>,
    rewrite_path: Option<RewriteParts>,
    rewrite_port: Option<u16>,
    headers_request: Rc<[HeaderEdit]>,
    headers_response: Rc<[HeaderEdit]>,
    tags: Option<Rc<CachedTags>>,
}

impl Frontend {
    pub fn new(
        domain_rule: &DomainRule,
        path_rule: &PathRule,
        front: &HttpFrontend,
    ) -> Result<Self, RouterError> {
        let cluster_id = front.cluster_id.clone();
        let required_auth = front.required_auth;
        let rewrite_port = front.rewrite_port;
        let rewrite_path = front.rewrite_path.clone();
        let rewrite_host = front.rewrite_host.clone();
        let redirect = front.redirect;
        let redirect_scheme = front.redirect_scheme;
        let redirect_template = front.redirect_template.clone();
        let headers = &front.headers;
        let tags = front
            .tags
            .clone()
            .map(|tags| Rc::new(CachedTags::new(tags)));

        let deny = match (&cluster_id, redirect, &redirect_template, required_auth) {
            (_, RedirectPolicy::Unauthorized, _, false) => true,
            (_, RedirectPolicy::Unauthorized, _, true) => {
                warn!("Frontend[cluster: {:?}, domain: {:?}, path: {:?}, redirect: {:?}]: unauthorized frontends ignore auth", cluster_id, domain_rule, path_rule, redirect);
                true
            }
            (None, RedirectPolicy::Forward, None, _) => {
                warn!("Frontend[domain: {:?}, path: {:?}]: forward on clusterless frontends are unauthorized", domain_rule, path_rule);
                true
            }
            (None, _, _, true) => {
                warn!(
                    "Frontend[domain: {:?}, path: {:?}]: clusterless frontends ignore auth",
                    domain_rule, path_rule
                );
                true
            }
            _ => false,
        };
        if deny {
            return Ok(Self {
                cluster_id: cluster_id.clone(),
                required_auth,
                redirect: RedirectPolicy::Unauthorized,
                redirect_scheme,
                redirect_template: None,
                capture_cap_host: 0,
                capture_cap_path: 0,
                rewrite_host: None,
                rewrite_path: None,
                rewrite_port: None,
                headers_request: Rc::new([]),
                headers_response: Rc::new([]),
                tags: None,
            });
        }
        let mut capture_cap_host = match domain_rule {
            DomainRule::Any => 1,
            DomainRule::Equals(_) => 1,
            DomainRule::Wildcard(_) => 2,
            DomainRule::Regex(regex) => regex.captures_len(),
        };
        let mut capture_cap_path = match path_rule {
            PathRule::Equals(_) => 1,
            PathRule::Prefix(_) => 2,
            PathRule::Regex(regex) => regex.captures_len(),
        };
        let mut used_capture_host = 0;
        let mut used_capture_path = 0;
        let rewrite_host = if let Some(p) = rewrite_host {
            Some(
                RewriteParts::new(
                    &p,
                    capture_cap_host,
                    capture_cap_path,
                    &mut used_capture_host,
                    &mut used_capture_path,
                )
                .ok_or(RouterError::InvalidHostRewrite(p))?,
            )
        } else {
            None
        };
        let rewrite_path = if let Some(p) = rewrite_path {
            Some(
                RewriteParts::new(
                    &p,
                    capture_cap_host,
                    capture_cap_path,
                    &mut used_capture_host,
                    &mut used_capture_path,
                )
                .ok_or(RouterError::InvalidPathRewrite(p))?,
            )
        } else {
            None
        };
        if used_capture_host == 0 {
            capture_cap_host = 0;
        }
        if used_capture_path == 0 {
            capture_cap_path = 0;
        }
        let mut headers_request = Vec::new();
        let mut headers_response = Vec::new();
        for header in headers {
            if header.position() == Position::Frontend {
                headers_request.push(HeaderEdit {
                    key: header.key.clone().into_bytes().into(),
                    val: header.val.clone().into_bytes().into(),
                });
            } else {
                headers_response.push(HeaderEdit {
                    key: header.key.clone().into_bytes().into(),
                    val: header.val.clone().into_bytes().into(),
                });
            }
        }
        Ok(Frontend {
            cluster_id,
            required_auth,
            redirect,
            redirect_scheme,
            redirect_template,
            capture_cap_host,
            capture_cap_path,
            rewrite_host,
            rewrite_path,
            rewrite_port,
            headers_request: headers_request.into(),
            headers_response: headers_response.into(),
            tags,
        })
    }

    #[cfg(test)]
    pub fn forward(cluster_id: ClusterId) -> Self {
        Self {
            cluster_id: Some(cluster_id),
            required_auth: false,
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::new([]),
            tags: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteResult {
    pub cluster_id: Option<ClusterId>,
    pub required_auth: bool,
    pub redirect: RedirectPolicy,
    pub redirect_scheme: RedirectScheme,
    pub redirect_template: Option<String>,
    pub rewritten_host: Option<String>,
    pub rewritten_path: Option<String>,
    pub rewritten_port: Option<u16>,
    pub headers_request: Rc<[HeaderEdit]>,
    pub headers_response: Rc<[HeaderEdit]>,
    pub tags: Option<Rc<CachedTags>>,
}

impl RouteResult {
    fn deny(cluster_id: &Option<ClusterId>) -> Self {
        Self {
            cluster_id: cluster_id.clone(),
            required_auth: false,
            redirect: RedirectPolicy::Unauthorized,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            rewritten_host: None,
            rewritten_path: None,
            rewritten_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::new([]),
            tags: None,
        }
    }

    fn new<'a>(
        captures_host: Vec<&'a str>,
        path: &'a [u8],
        path_rule: &PathRule,
        route: &Frontend,
    ) -> Self {
        let Frontend {
            cluster_id,
            required_auth,
            redirect,
            redirect_scheme,
            redirect_template,
            capture_cap_path,
            rewrite_host,
            rewrite_path,
            rewrite_port,
            headers_request,
            headers_response,
            tags,
            ..
        } = route;
        let mut captures_path = Vec::with_capacity(*capture_cap_path);
        if *capture_cap_path > 0 {
            captures_path.push(unsafe { from_utf8_unchecked(path) });
            match path_rule {
                PathRule::Prefix(prefix) => {
                    captures_path.push(unsafe { from_utf8_unchecked(&path[prefix.len()..]) })
                }
                PathRule::Regex(regex) => captures_path.extend(
                    regex
                        .captures(path)
                        .unwrap()
                        .iter()
                        .map(|c| unsafe { from_utf8_unchecked(c.unwrap().as_bytes()) }),
                ),
                _ => {}
            }
        }
        // println!("========HOST_CAPTURES: {captures_host:?}");
        // println!("========PATH_CAPTURES: {captures_path:?}");
        Self {
            cluster_id: cluster_id.clone(),
            required_auth: *required_auth,
            redirect: *redirect,
            redirect_scheme: *redirect_scheme,
            redirect_template: redirect_template.clone(),
            rewritten_host: rewrite_host
                .as_ref()
                .map(|rewrite| rewrite.run(&captures_host, &captures_path)),
            rewritten_path: rewrite_path
                .as_ref()
                .map(|rewrite| rewrite.run(&captures_host, &captures_path)),
            rewritten_port: *rewrite_port,
            headers_request: headers_request.clone(),
            headers_response: headers_response.clone(),
            tags: tags.clone(),
        }
    }
    fn new_no_trie<'a>(
        domain: &'a [u8],
        domain_rule: &DomainRule,
        path: &'a [u8],
        path_rule: &PathRule,
        route: &Frontend,
    ) -> Self {
        let Frontend {
            cluster_id,
            redirect,
            capture_cap_host,
            ..
        } = route;
        if *redirect == RedirectPolicy::Unauthorized {
            return Self::deny(cluster_id);
        }
        let mut captures_host = Vec::with_capacity(*capture_cap_host);
        if *capture_cap_host > 0 {
            captures_host.push(unsafe { from_utf8_unchecked(domain) });
            match domain_rule {
                DomainRule::Wildcard(suffix) => captures_host
                    .push(unsafe { from_utf8_unchecked(&domain[..domain.len() - suffix.len()]) }),
                DomainRule::Regex(regex) => captures_host.extend(
                    regex
                        .captures(domain)
                        .unwrap()
                        .iter()
                        .skip(1)
                        .map(|c| unsafe { from_utf8_unchecked(c.unwrap().as_bytes()) }),
                ),
                _ => {}
            }
        }
        Self::new(captures_host, path, path_rule, route)
    }
    fn new_with_trie<'a>(
        domain: &'a [u8],
        domain_submatches: TrieMatches<'_, 'a>,
        path: &'a [u8],
        path_rule: &PathRule,
        route: &Frontend,
    ) -> Self {
        let Frontend {
            cluster_id,
            redirect,
            capture_cap_host,
            ..
        } = route;
        if *redirect == RedirectPolicy::Unauthorized {
            return Self::deny(cluster_id);
        }
        let mut captures_host = Vec::with_capacity(*capture_cap_host);
        if *capture_cap_host > 0 {
            captures_host.push(unsafe { from_utf8_unchecked(domain) });
            for submatch in domain_submatches {
                match submatch {
                    TrieSubMatch::Wildcard(part) => {
                        captures_host.push(unsafe { from_utf8_unchecked(part) })
                    }
                    TrieSubMatch::Regexp(part, regex) => captures_host.extend(
                        regex
                            .captures(part)
                            .unwrap()
                            .iter()
                            .skip(1)
                            .map(|c| unsafe { from_utf8_unchecked(c.unwrap().as_bytes()) }),
                    ),
                }
            }
        }
        Self::new(captures_host, path, path_rule, route)
    }
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
            DomainRule::Equals("www.example.com".to_string())
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
            DomainRule::Equals("www.example.com".to_string()).matches("www.example.com".as_bytes())
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

    ///  [io]
    ///      \
    ///       [sozu]
    ///             \
    ///              [*]  <- this wildcard has multiple children
    ///             /   \
    ///         (base) (api)
    #[test]
    fn multiple_children_on_a_wildcard() {
        let mut router = Router::new();

        assert!(router.add_tree_rule(
            b"*.sozu.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("base".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router
                .lookup("www.sozu.io", "/api", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("base".to_string()))
        );
        assert!(router.add_tree_rule(
            b"*.sozu.io",
            &PathRule::Prefix("/api".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("api".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router
                .lookup("www.sozu.io", "/ap", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("base".to_string()))
        );
        assert_eq!(
            router
                .lookup("www.sozu.io", "/api", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("api".to_string()))
        );
    }

    ///  [io]
    ///      \
    ///       [sozu]  <- this node has multiple children including a wildcard
    ///      /      \
    ///   (api)      [*]  <- this wildcard has multiple children
    ///                 \
    ///                (base)
    #[test]
    fn multiple_children_including_one_with_wildcard() {
        let mut router = Router::new();

        assert!(router.add_tree_rule(
            b"*.sozu.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("base".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router
                .lookup("www.sozu.io", "/api", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("base".to_string()))
        );
        assert!(router.add_tree_rule(
            b"api.sozu.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("api".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router
                .lookup("www.sozu.io", "/api", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("base".to_string()))
        );
        assert_eq!(
            router
                .lookup("api.sozu.io", "/api", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("api".to_string()))
        );
    }

    #[test]
    fn router_insert_remove_through_regex() {
        let mut router = Router::new();

        assert!(router.add_tree_rule(
            b"www./.*/.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("base".to_string())
        ));
        println!("{:#?}", router.tree);
        assert!(router.add_tree_rule(
            b"www.doc./.*/.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("doc".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router
                .lookup("www.sozu.io", "/", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("base".to_string()))
        );
        assert_eq!(
            router
                .lookup("www.doc.sozu.io", "/", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("doc".to_string()))
        );
        assert!(router.remove_tree_rule(
            b"www./.*/.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string()))
        ));
        println!("{:#?}", router.tree);
        assert!(router.lookup("www.sozu.io", "/", &Method::Get).is_err());
        assert_eq!(
            router
                .lookup("www.doc.sozu.io", "/", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("doc".to_string()))
        );
    }

    #[test]
    fn match_router() {
        let mut router = Router::new();

        assert!(router.add_pre_rule(
            &"*".parse::<DomainRule>().unwrap(),
            &PathRule::Prefix("/.well-known/acme-challenge".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("acme".to_string())
        ));
        assert!(router.add_tree_rule(
            "www.example.com".as_bytes(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("example".to_string())
        ));
        assert!(router.add_tree_rule(
            "*.test.example.com".as_bytes(),
            &PathRule::Regex(Regex::new("/hello[A-Z]+/").unwrap()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("examplewildcard".to_string())
        ));
        assert!(router.add_tree_rule(
            "/test[0-9]/.example.com".as_bytes(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Frontend::forward("exampleregex".to_string())
        ));

        assert_eq!(
            router
                .lookup("www.example.com", "/helloA", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("example".to_string()))
        );
        assert_eq!(
            router
                .lookup(
                    "www.example.com",
                    "/.well-known/acme-challenge",
                    &Method::Get
                )
                .map(|r| r.cluster_id),
            Ok(Some("acme".to_string()))
        );
        assert!(router
            .lookup("www.test.example.com", "/", &Method::Get)
            .is_err());
        assert_eq!(
            router
                .lookup("www.test.example.com", "/helloAB/", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("examplewildcard".to_string()))
        );
        assert_eq!(
            router
                .lookup("test1.example.com", "/helloAB/", &Method::Get)
                .map(|r| r.cluster_id),
            Ok(Some("exampleregex".to_string()))
        );
    }
}
