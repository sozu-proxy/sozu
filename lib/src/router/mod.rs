pub mod pattern_trie;

use std::{
    fmt::{self, Debug, Write},
    rc::Rc,
    str::from_utf8,
    time::Instant,
};

use regex::bytes::Regex;
use sozu_command::{
    logging::CachedTags,
    proto::command::{
        HeaderPosition, HstsConfig, PathRule as CommandPathRule, PathRuleKind, RedirectPolicy,
        RedirectScheme, RulePosition,
    },
    response::HttpFrontend,
    state::ClusterId,
};

use crate::metrics::names;
use crate::{
    protocol::{http::editor::HeaderEditMode, http::parser::Method},
    router::pattern_trie::{InsertResult, TrieMatches, TrieNode, TrieSubMatch},
    sozu_command::logging::ansi_palette,
};

/// Module-level prefix tag for `lib/src/router/`. Honours the runtime
/// colored-output flag via [`ansi_palette`]; consumed by the single
/// `warn!` site in `Frontend::new` (and any future emitter without an
/// `HttpContext` in scope) so static log-layout regression checks
/// (`lib/tests/log_layout.rs`) keep router log lines on the canonical
/// `[ROUTER] >>>` envelope.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}ROUTER{reset}\t >>>", open = open, reset = reset)
    }};
}

/// Upper bound (in bytes) on a frontend hostname accepted by the router,
/// checked by [`Router::add_http_front_with_hsts_origin`] and
/// [`Router::remove_http_front`] before anything parses the hostname.
///
/// RFC 1035 caps a domain name at 255 octets; Sōzu's regex-segment
/// grammar (`/re/.example.com`) can legitimately exceed that, so this is
/// a deliberately generous safety net rather than a strict RFC bound. It
/// exists because the route-table trie recurses once per label — a
/// hostname with ~100k labels aborts the worker with an uncatchable
/// stack overflow — and because the regex compilation a `/`-segment
/// triggers costs time linear in the pattern size (a 2 MiB hostname
/// stalls the single-threaded worker for ~855 ms before the regex size
/// limit rejects it).
pub const MAX_HOSTNAME_LENGTH: usize = 4096;

#[derive(thiserror::Error, PartialEq)]
pub enum RouterError {
    #[error("Could not parse rule from frontend path, path_bytes={}", .0.len())]
    InvalidPathRule(String),
    #[error("parsing hostname failed, hostname_bytes={}", .hostname.len())]
    InvalidDomain { hostname: String },
    #[error("Could not parse host rewrite, rewrite_host_bytes={}", .0.len())]
    InvalidHostRewrite(String),
    #[error("Could not parse path rewrite, rewrite_path_bytes={}", .0.len())]
    InvalidPathRewrite(String),
    #[error("Could not add route, route_bytes={}", .0.len())]
    AddRoute(String),
    #[error("Could not remove route, route_bytes={}", .0.len())]
    RemoveRoute(String),
    #[error("route_not_found method={method:?} host_bytes={} path_bytes={}", .host.len(), .path.len())]
    RouteNotFound {
        host: String,
        path: String,
        method: Method,
    },
}

impl fmt::Debug for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
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

    /// Resolve a request to a [`RouteResult`].
    ///
    /// Looks up `(hostname, path, method)` against the pre, tree, and post
    /// rule lists. The matched [`Route`] is converted into a [`RouteResult`]:
    /// legacy variants ([`Route::ClusterId`], [`Route::Deny`]) synthesize a
    /// minimal `RouteResult` so existing call sites keep working, while
    /// [`Route::Frontend`] runs the full Frontend → RouteResult pipeline,
    /// substituting host/path captures into [`RewriteParts`] templates.
    pub fn lookup(
        &self,
        hostname: &str,
        path: &str,
        method: &Method,
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

        let trie_path: TrieMatches<'_, '_> = Vec::with_capacity(16);
        if let Some(((_, path_rules), trie_matches)) =
            self.tree.lookup_with_path(hostname_b, true, trie_path)
        {
            let mut prefix_length = 0;
            let mut matched: Option<(&PathRule, &Route)> = None;

            for (rule, method_rule, route) in path_rules {
                match rule.matches(path_b) {
                    PathRuleResult::Regex | PathRuleResult::Equals => {
                        match method_rule.matches(method) {
                            MethodRuleResult::Equals => {
                                return Ok(RouteResult::new_with_trie(
                                    hostname_b,
                                    trie_matches,
                                    path_b,
                                    rule,
                                    route,
                                ));
                            }
                            MethodRuleResult::All => {
                                prefix_length = path_b.len();
                                matched = Some((rule, route));
                            }
                            MethodRuleResult::None => {}
                        }
                    }
                    PathRuleResult::Prefix(size) => {
                        if size >= prefix_length {
                            match method_rule.matches(method) {
                                // FIXME: the rule order will be important here
                                MethodRuleResult::Equals => {
                                    // Longest-prefix wins: the selected
                                    // length is monotonically non-decreasing
                                    // across the candidate scan.
                                    debug_assert!(
                                        size >= prefix_length,
                                        "longest-prefix selection must never shrink the match length",
                                    );
                                    prefix_length = size;
                                    matched = Some((rule, route));
                                }
                                MethodRuleResult::All => {
                                    debug_assert!(
                                        size >= prefix_length,
                                        "longest-prefix selection must never shrink the match length",
                                    );
                                    prefix_length = size;
                                    matched = Some((rule, route));
                                }
                                MethodRuleResult::None => {}
                            }
                        }
                    }
                    PathRuleResult::None => {}
                }
            }

            if let Some((path_rule, route)) = matched {
                return Ok(RouteResult::new_with_trie(
                    hostname_b,
                    trie_matches,
                    path_b,
                    path_rule,
                    route,
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

    /// Add an HTTP/HTTPS frontend whose `hsts` field (if any) came from
    /// the per-frontend configuration directly. Equivalent to
    /// [`Self::add_http_front_with_hsts_origin`] called with
    /// [`HstsOrigin::Explicit`]. The default for callers that don't
    /// know about listener-default inheritance — e.g. plain HTTP
    /// listeners (`HttpListenerConfig` has no HSTS field) and tests.
    pub fn add_http_front(&mut self, front: &HttpFrontend) -> Result<(), RouterError> {
        self.add_http_front_with_hsts_origin(front, HstsOrigin::Explicit)
    }

    /// Add an HTTP/HTTPS frontend, recording whether the resolved
    /// `front.hsts` was inherited from the listener default. The
    /// inheritance bit is preserved on the resulting [`Frontend`] so a
    /// later `UpdateHttpsListenerConfig.hsts` patch can reflow the new
    /// default onto inheriting entries without disturbing explicit
    /// per-frontend overrides.
    pub fn add_http_front_with_hsts_origin(
        &mut self,
        front: &HttpFrontend,
        hsts_origin: HstsOrigin,
    ) -> Result<(), RouterError> {
        // Bounded BEFORE any parse: the `DomainRule` parse below compiles
        // control-plane-supplied regex segments, and the trie recurses
        // once per label (see `MAX_HOSTNAME_LENGTH`).
        if front.hostname.len() > MAX_HOSTNAME_LENGTH {
            return Err(RouterError::InvalidDomain {
                hostname: front.hostname.clone(),
            });
        }

        let path_rule = PathRule::from_config(front.path.clone())
            .ok_or(RouterError::InvalidPathRule(front.path.to_string()))?;

        let method_rule = MethodRule::new(front.method.clone());

        // Decide between the legacy `Route::ClusterId`/`Route::Deny` shape
        // and the rich `Route::Frontend(Rc<Frontend>)` shape: any non-
        // default policy field flips us onto the rich path so the mux
        // can honour redirect/rewrite/headers/auth at request time.
        let has_policy = front.redirect.is_some()
            || front.redirect_scheme.is_some()
            || front.redirect_template.is_some()
            || front.rewrite_host.is_some()
            || front.rewrite_path.is_some()
            || front.rewrite_port.is_some()
            || front.required_auth.unwrap_or(false)
            || !front.headers.is_empty()
            || front.hsts.is_some();

        let domain =
            front
                .hostname
                .parse::<DomainRule>()
                .map_err(|_| RouterError::InvalidDomain {
                    hostname: front.hostname.clone(),
                })?;

        let route = if has_policy {
            let redirect = front
                .redirect
                .and_then(|r| RedirectPolicy::try_from(r).ok())
                .unwrap_or(RedirectPolicy::Forward);
            let redirect_scheme = front
                .redirect_scheme
                .and_then(|s| RedirectScheme::try_from(s).ok())
                .unwrap_or(RedirectScheme::UseSame);
            let frontend = Frontend::new(
                &domain,
                &path_rule,
                front,
                redirect,
                redirect_scheme,
                front.redirect_template.clone(),
                front.rewrite_host.clone(),
                front.rewrite_path.clone(),
                front.rewrite_port.and_then(|p| u16::try_from(p).ok()),
                &front.headers,
                front.required_auth.unwrap_or(false),
                hsts_origin,
            )?;
            Route::Frontend(Rc::new(frontend))
        } else {
            match &front.cluster_id {
                Some(cluster_id) => Route::ClusterId(cluster_id.clone()),
                None => Route::Deny,
            }
        };

        let success = match front.position {
            RulePosition::Pre => self.add_pre_rule(&domain, &path_rule, &method_rule, &route),
            RulePosition::Post => self.add_post_rule(&domain, &path_rule, &method_rule, &route),
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
        // Same bound as `add_http_front_with_hsts_origin`: the Pre/Post
        // arms below re-parse the hostname into a `DomainRule`.
        if front.hostname.len() > MAX_HOSTNAME_LENGTH {
            return Err(RouterError::InvalidDomain {
                hostname: front.hostname.clone(),
            });
        }

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
                if let Some((_, paths)) = self.tree.domain_lookup_mut(hostname.as_bytes(), false) {
                    empty = false;
                    let before = paths.len();
                    if !paths.iter().any(|(p, m, _)| p == path && m == method) {
                        paths.push((path.to_owned(), method.to_owned(), cluster.to_owned()));
                        // Append must add exactly one (path, method) leaf
                        // and the new rule must now be present.
                        debug_assert_eq!(
                            paths.len(),
                            before + 1,
                            "appending a tree rule must grow the leaf's rule list by exactly one",
                        );
                        debug_assert!(
                            paths.iter().any(|(p, m, _)| p == path && m == method),
                            "the freshly appended (path, method) rule must be present after insert",
                        );
                        return true;
                    }
                }

                if empty {
                    // Snapshot the ASCII host bytes before the move so the
                    // post-insert reachability check can re-look-up the
                    // domain. Ungated `let` (read only inside the gated
                    // assert) → dropped by the optimizer in release.
                    let inserted_host = hostname.clone().into_bytes();
                    let insert_result = self.tree.domain_insert(
                        hostname.into_bytes(),
                        vec![(path.to_owned(), method.to_owned(), cluster.to_owned())],
                    );
                    // A malformed hostname reaches us straight from the
                    // control plane (`AddHttpFrontend` over the command
                    // socket, or a `LoadState` replay), so the route table
                    // rejecting it is an expected outcome, not a Sozu bug:
                    // report the failure and let the caller answer the
                    // request with an error. Shapes the trie rejects
                    // include a host ending in `/` with no openable regex
                    // segment (`example.com/`), a regex segment that is not
                    // `.`-anchored (`abc/[0-9]+/.example.com`), a segment
                    // that is not a valid regex (`/[/.example.com`), and an
                    // empty label (`.example.com`).
                    if insert_result == InsertResult::Failed {
                        // Redacted like every other router log site: the
                        // shape is what matters here. `RouterError::AddRoute`
                        // is redacting too (`route_bytes=<len>`), so the
                        // hostname itself is only visible where the main
                        // process audit-logs the request it fanned out
                        // (`bin/src/command/requests.rs`) -- correlate by
                        // timestamp to identify the offending frontend.
                        error!(
                            "{} the route table rejected a malformed hostname, hostname_bytes={}",
                            log_module_context!(),
                            inserted_host.len(),
                        );
                        return false;
                    }
                    // A fresh domain must now be reachable, carrying the
                    // single rule just inserted. Use `domain_lookup_mut`
                    // (not the immutable `domain_lookup`): only the `_mut`
                    // resolver handles a literal wildcard key (`*.sozu.io`)
                    // via its `partial_key == b"*"` segment case, which is
                    // exactly the resolution the append branch above relies
                    // on. The immutable `lookup` lacks that case and would
                    // miss wildcard entries.
                    debug_assert!(
                        self.tree
                            .domain_lookup_mut(&inserted_host, false)
                            .is_some_and(|(_, paths)| paths
                                .iter()
                                .any(|(p, m, _)| p == path && m == method)),
                        "a freshly inserted tree domain must resolve to its inserted rule",
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
                        // `retain` evicts every matching (path, method)
                        // rule; none may survive the filter.
                        debug_assert!(
                            !paths.iter().any(|(p, m, _)| p == path && m == method),
                            "remove must evict every matching (path, method) rule from the leaf",
                        );
                    }

                    paths_opt
                        .as_ref()
                        .map(|(_, paths)| paths.is_empty())
                        .unwrap_or(false)
                };

                if should_delete {
                    let removed_host = hostname.clone().into_bytes();
                    self.tree.domain_remove(&hostname.into_bytes());
                    // Dropping the last rule must make the whole domain
                    // unreachable — no stranded empty leaf left behind.
                    // `domain_lookup_mut` resolves literal wildcard keys
                    // (`*.sozu.io`), so this genuinely verifies wildcard
                    // entries are gone too (the immutable `lookup` lacks
                    // the `partial_key == b"*"` case and would always read
                    // None for a wildcard host, weakening the check).
                    debug_assert!(
                        self.tree.domain_lookup_mut(&removed_host, false).is_none(),
                        "a domain whose last rule was removed must be unreachable",
                    );
                }

                true
            }
            Err(_) => false,
        }
    }

    /// Walk every route and re-materialise the response-side HSTS edit
    /// on frontends that inherited from the listener default. Operator
    /// per-frontend HSTS overrides (`inherits_listener_hsts == false`)
    /// are left untouched.
    ///
    /// Called from `lib/src/https.rs::HttpsListener::update_config` when
    /// an `UpdateHttpsListenerConfig.hsts` patch is applied.
    ///
    /// Two refresh paths:
    ///
    /// 1. **`Route::Frontend(rc)` with `inherits_listener_hsts == true`**:
    ///    rebuild `headers_response` by dropping any existing
    ///    `Strict-Transport-Security` entry and appending a freshly
    ///    rendered one when `new_hsts` resolves to an enabled value
    ///    (`enabled = Some(true)`). The existing operator
    ///    `Append`/`Set` response headers stay in place.
    ///
    /// 2. **`Route::ClusterId(id)` and `Route::Deny`** (lightweight
    ///    "no policy" shapes): when `new_hsts` resolves to enabled,
    ///    promote in place to a minimal `Route::Frontend(rc)` carrying
    ///    just the HSTS edit on `headers_response` (and
    ///    `inherits_listener_hsts == true` so subsequent patches keep
    ///    refreshing the entry). Routing semantics are preserved — the
    ///    promoted Frontend forwards / denies identically to the
    ///    original variant — and the promoted entry now participates
    ///    in path 1 on every later patch. When `new_hsts` resolves to
    ///    "no HSTS" (None / disabled), lightweight routes are left
    ///    untouched (no allocation is created just to hold an empty
    ///    HSTS edit).
    ///
    /// Path 2 fixes the case where a frontend was added without any
    /// per-frontend policy field (the routing fast path stores it as
    /// `Route::ClusterId` / `Route::Deny`, NOT `Route::Frontend`). Before
    /// this two-path walk, listener-default HSTS patches silently
    /// skipped every such "no-policy" frontend — which on a typical
    /// Clever Cloud `cleverapps.io` shared listener was 99 % of the
    /// frontends.
    ///
    /// Returns the number of frontends touched. For path 1, refreshed
    /// frontends where the new policy resolves to "no HSTS" are still
    /// counted (the existing HSTS edit is stripped). For path 2, only
    /// frontends actually promoted (i.e. `new_hsts` enabled) are
    /// counted, since "no HSTS" is a no-op on the lightweight shape.
    pub fn refresh_inheriting_hsts(&mut self, new_hsts: Option<&HstsConfig>) -> usize {
        let mut refreshed = 0usize;
        // Pre-compute the listener-default HSTS edit ONCE so every
        // visited frontend in this patch shares the same `Rc`-backed
        // key / val allocation. `Some(_)` doubles as the "promote
        // lightweight routes" gate — there is no point allocating a
        // promoted Frontend just to hold an empty headers_response.
        // See `build_listener_hsts_edit`'s rustdoc for the
        // ~1.5 M-allocation-per-worker savings on cleverapps.io.
        let new_edit = build_listener_hsts_edit(new_hsts);
        let new_edit_ref = new_edit.as_ref();
        let promote_lightweight = new_edit_ref.is_some();
        let mut visit = |route: &mut Route| match route {
            Route::Frontend(rc) => {
                if rc.inherits_listener_hsts {
                    let new_frontend = rebuild_with_listener_hsts(rc, new_edit_ref);
                    *rc = Rc::new(new_frontend);
                    refreshed += 1;
                }
            }
            Route::ClusterId(id) => {
                if promote_lightweight {
                    let promoted = rebuild_with_listener_hsts(
                        &Frontend::minimal_forward(id.clone()),
                        new_edit_ref,
                    );
                    *route = Route::Frontend(Rc::new(promoted));
                    refreshed += 1;
                }
            }
            Route::Deny => {
                if promote_lightweight {
                    let promoted =
                        rebuild_with_listener_hsts(&Frontend::minimal_deny(), new_edit_ref);
                    *route = Route::Frontend(Rc::new(promoted));
                    refreshed += 1;
                }
            }
        };

        for (_, _, _, route) in self.pre.iter_mut() {
            visit(route);
        }
        self.tree.for_each_value_mut(&mut |paths| {
            for (_, _, route) in paths.iter_mut() {
                visit(route);
            }
        });
        for (_, _, _, route) in self.post.iter_mut() {
            visit(route);
        }
        refreshed
    }

    pub fn add_pre_rule(
        &mut self,
        domain: &DomainRule,
        path: &PathRule,
        method: &MethodRule,
        cluster_id: &Route,
    ) -> bool {
        let before = self.pre.len();
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
            // A new pre-rule grows the list by exactly one and is now
            // present (dedup of the same triple is the caller's `false`
            // path, not this one).
            debug_assert_eq!(
                self.pre.len(),
                before + 1,
                "adding a unique pre-rule must push exactly one entry",
            );
            debug_assert!(
                self.pre
                    .iter()
                    .any(|(d, p, m, _)| d == domain && p == path && m == method),
                "the freshly added pre-rule must be present",
            );
            true
        } else {
            debug_assert_eq!(
                self.pre.len(),
                before,
                "a duplicate pre-rule must not change the list length",
            );
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
        let before = self.post.len();
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
            debug_assert_eq!(
                self.post.len(),
                before + 1,
                "adding a unique post-rule must push exactly one entry",
            );
            debug_assert!(
                self.post
                    .iter()
                    .any(|(d, p, m, _)| d == domain && p == path && m == method),
                "the freshly added post-rule must be present",
            );
            true
        } else {
            debug_assert_eq!(
                self.post.len(),
                before,
                "a duplicate post-rule must not change the list length",
            );
            false
        }
    }

    pub fn remove_pre_rule(
        &mut self,
        domain: &DomainRule,
        path: &PathRule,
        method: &MethodRule,
    ) -> bool {
        let before = self.pre.len();
        match self
            .pre
            .iter()
            .position(|(d, p, m, _)| d == domain && p == path && m == method)
        {
            None => {
                debug_assert_eq!(
                    self.pre.len(),
                    before,
                    "a no-op pre-rule removal must not change the list length",
                );
                false
            }
            Some(index) => {
                debug_assert!(index < self.pre.len(), "found index must be in bounds");
                self.pre.remove(index);
                // Exactly one entry left, and the triple is now gone.
                debug_assert_eq!(
                    self.pre.len() + 1,
                    before,
                    "removing a pre-rule must drop exactly one entry",
                );
                debug_assert!(
                    !self
                        .pre
                        .iter()
                        .any(|(d, p, m, _)| d == domain && p == path && m == method),
                    "the removed pre-rule must no longer be present",
                );
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
        let before = self.post.len();
        match self
            .post
            .iter()
            .position(|(d, p, m, _)| d == domain && p == path && m == method)
        {
            None => {
                debug_assert_eq!(
                    self.post.len(),
                    before,
                    "a no-op post-rule removal must not change the list length",
                );
                false
            }
            Some(index) => {
                debug_assert!(index < self.post.len(), "found index must be in bounds");
                self.post.remove(index);
                debug_assert_eq!(
                    self.post.len() + 1,
                    before,
                    "removing a post-rule must drop exactly one entry",
                );
                debug_assert!(
                    !self
                        .post
                        .iter()
                        .any(|(d, p, m, _)| d == domain && p == path && m == method),
                    "the removed post-rule must no longer be present",
                );
                true
            }
        }
    }

    /// Returns true if any route (pre, tree, or post) references the given hostname.
    ///
    /// This is used after removing a frontend to decide whether the hostname's
    /// tags should be cleaned up. Tags must only be removed when no routes remain.
    pub fn has_hostname(&self, hostname: &str) -> bool {
        let hostname_b = hostname.as_bytes();

        // Check pre rules
        for (domain_rule, _, _, _) in &self.pre {
            if domain_rule.matches(hostname_b) {
                return true;
            }
        }

        // Check tree rules (exact match only, no wildcard resolution)
        if let Ok(ascii_hostname) = ::idna::domain_to_ascii(hostname)
            && self
                .tree
                .domain_lookup(ascii_hostname.as_bytes(), false)
                .is_some()
        {
            return true;
        }

        // Check post rules
        for (domain_rule, _, _, _) in &self.post {
            if domain_rule.matches(hostname_b) {
                return true;
            }
        }

        false
    }
}

#[derive(Clone, Debug)]
pub enum DomainRule {
    Any,
    Exact(String),
    /// Matches when `hostname` ends with `s[1..]` (the wildcard pattern with
    /// the leading `*` stripped) and the remaining leftmost prefix is
    /// non-empty and contains no `.`. Comparison is byte-exact and
    /// case-sensitive; no IDN/punycode normalisation is performed here.
    /// Stored with the leading `*`.
    Wildcard(String),
    Regex(Regex),
}

fn convert_regex_domain_rule(hostname: &str) -> Option<String> {
    // Anchor at both ends so `Regex::is_match` only succeeds on a full-host
    // match. Without `\A` the pattern `/example\.com/` matches any hostname
    // containing `example.com` as a substring (e.g. `attacker.example.com.evil.org`),
    // letting an attacker-controlled domain reach a frontend that should only
    // serve `example.com`.
    let mut result = String::from("\\A");

    let s = hostname.as_bytes();
    let mut index = 0;
    loop {
        // A bare trailing `.` after a completed segment (`/a/.`, `x./y/.`)
        // leaves `index` one past the end through the `index += 1` in the
        // loop tail; indexing `s[index]` here would then panic (slice
        // bounds checks never compile out of release). The grammar
        // requires a label after every `.`, so reject instead.
        if index >= s.len() {
            return None;
        }
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
            result.push_str("\\z");
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
                // A stored wildcard always keeps its leading `*`, so the
                // suffix (pattern minus `*`) is a strict sub-slice and the
                // bare `*` (Any) never reaches this arm.
                debug_assert_eq!(
                    s.as_bytes().first(),
                    Some(&b'*'),
                    "a Wildcard rule must retain its leading '*'",
                );
                let suffix = &s.as_bytes()[1..];
                let matched = hostname
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| !prefix.is_empty() && !prefix.contains(&b'.'));
                // A wildcard never matches a hostname no longer than its
                // own suffix — there is no room left for the mandatory
                // single non-empty leftmost label.
                debug_assert!(
                    !matched || hostname.len() > suffix.len(),
                    "a wildcard match requires a non-empty leftmost label before the suffix",
                );
                matched
            }
            DomainRule::Exact(s) => s.as_bytes() == hostname,
            DomainRule::Regex(r) => {
                let start = Instant::now();
                let is_a_match = r.is_match(hostname);
                let now = Instant::now();
                time!(
                    names::event_loop::REGEX_MATCHING_TIME,
                    (now - start).as_millis()
                );
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
            PathRule::Prefix(prefix) => {
                if path.starts_with(prefix.as_bytes()) {
                    // The reported prefix length is the matched-byte count
                    // the router uses for longest-prefix tie-breaking; it
                    // must equal the prefix and never exceed the path.
                    debug_assert!(
                        prefix.len() <= path.len(),
                        "a matching prefix cannot be longer than the path it matched",
                    );
                    PathRuleResult::Prefix(prefix.len())
                } else {
                    PathRuleResult::None
                }
            }
            PathRule::Regex(regex) => {
                let start = Instant::now();
                let is_a_match = regex.is_match(path);
                let now = Instant::now();
                time!(
                    names::event_loop::REGEX_MATCHING_TIME,
                    (now - start).as_millis()
                );

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

/// `Regex` carries no `PartialEq`, so the comparison is written by hand
/// and compares patterns by `as_str()`. Every variant must answer for
/// itself: without an `Equals` arm the catch-all made two identical
/// `PathRule::Equals` unequal — not even reflexive — and the router's
/// bookkeeping (`add_*_rule` dedup, `remove_*_rule` eviction) silently
/// lost every `--path-equals` frontend, reporting success either way.
impl std::cmp::PartialEq for PathRule {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PathRule::Prefix(s1), PathRule::Prefix(s2)) => s1 == s2,
            (PathRule::Regex(r1), PathRule::Regex(r2)) => r1.as_str() == r2.as_str(),
            (PathRule::Equals(s1), PathRule::Equals(s2)) => s1 == s2,
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

/// What to do with a request that matches a frontend.
///
/// Three variants coexist today; the legacy two will retire once
/// `HttpFrontend` itself carries the rich routing fields:
///
/// - [`Route::ClusterId`] is the legacy "forward to this cluster" variant
///   used by call sites that build routes directly from
///   [`HttpFrontend::cluster_id`].
/// - [`Route::Deny`] is the legacy "send 401" variant used when a frontend
///   has no `cluster_id`.
/// - [`Route::Frontend`] carries a richer [`Frontend`] decision (redirect
///   policy, rewrite templates, header edits, auth gating). Once
///   `HttpFrontend` carries the matching proto fields, `add_http_front`
///   will build `Route::Frontend` directly and the two legacy variants
///   above can retire.
///
/// `Eq`/`PartialEq` compare `Frontend` variants by `Rc` pointer identity to
/// stay consistent with `Hash`/`Ord` on [`Rc`]; this is sufficient for the
/// router's de-duplication (`add_pre_rule`, `add_post_rule`,
/// `add_tree_rule`) which only checks against routes created from the same
/// configuration call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Route {
    /// send a 401 default answer
    Deny,
    /// the cluster to which the frontend belongs
    ClusterId(ClusterId),
    /// rich routing decision carrying redirect, rewrite, header, and auth
    /// configuration; supersedes the two legacy variants once the
    /// in-memory frontend wiring is migrated to build `Route::Frontend`
    /// directly.
    Frontend(Rc<Frontend>),
}

/// Materialise the listener-default HSTS into a single shareable
/// [`HeaderEdit`] when the supplied policy resolves to a non-empty
/// `Strict-Transport-Security` header.
///
/// Returns `None` when the listener has no HSTS configured
/// (`new_hsts.is_none()`), when HSTS is explicitly disabled
/// (`enabled = Some(false)`), or when the render fails because of a
/// missing `max_age` (`enabled = Some(true)` with `max_age = None`,
/// the malformed-IPC defense-in-depth gate). Mirrors the gate
/// previously embedded in `rebuild_with_listener_hsts`.
///
/// Used by [`Router::refresh_inheriting_hsts`] to:
/// - decide whether promoting a lightweight `Route::ClusterId` /
///   `Route::Deny` to `Route::Frontend` is worth doing (`Some` =
///   promote + counted; `None` = lightweight route untouched, no
///   allocation created just to hold an empty edit), AND
/// - **share the same `Rc`-backed `key` / `val` allocation across
///   every visited frontend in this patch**. Without sharing, each
///   refreshed frontend would allocate a fresh
///   `Rc::from(b"strict-transport-security")` and a fresh
///   `Rc::from(rendered.into_bytes())` — on a 91 k-frontend
///   `cleverapps.io` shared listener × 8 workers, one HSTS-enable
///   patch produces ~1.5 M identical-content `Rc` allocations per
///   worker. With sharing the cost collapses to one allocation per
///   patch plus a refcount bump on each frontend (`HeaderEdit::clone`
///   is two `Rc::clone`s + a byte copy).
fn build_listener_hsts_edit(new_hsts: Option<&HstsConfig>) -> Option<HeaderEdit> {
    let cfg = new_hsts?;
    if !matches!(cfg.enabled, Some(true)) {
        return None;
    }
    let rendered = render_hsts(cfg)?;
    let mode = if matches!(cfg.force_replace_backend, Some(true)) {
        HeaderEditMode::Set
    } else {
        HeaderEditMode::SetIfAbsent
    };
    Some(HeaderEdit {
        key: Rc::from(&b"strict-transport-security"[..]),
        val: rendered.into_bytes().into(),
        mode,
    })
}

/// Build a new [`Frontend`] cloned from `frontend`, with its
/// `headers_response` re-materialised against `new_edit` — the
/// shared listener-default HSTS edit pre-built by
/// [`build_listener_hsts_edit`]. Used by
/// [`Router::refresh_inheriting_hsts`].
///
/// Preserves every operator-defined response-header edit (`Append`,
/// `Set`, legacy empty-`val`-Append delete) and replaces any existing
/// `Strict-Transport-Security` entry with `new_edit`. When `new_edit`
/// is `None` (listener-default HSTS resolves to "no HSTS"), the
/// function strips the existing STS entry and adds nothing.
///
/// Preserves the existing `inherits_listener_hsts` marker; callers
/// ensure it is `true` before invoking this helper (the function
/// uses `..frontend.clone()` so it inherits whatever the input has).
fn rebuild_with_listener_hsts(frontend: &Frontend, new_edit: Option<&HeaderEdit>) -> Frontend {
    // Strip any existing Strict-Transport-Security entry.
    let mut headers_response: Vec<HeaderEdit> = frontend
        .headers_response
        .iter()
        .filter(|edit| !edit.key.eq_ignore_ascii_case(b"strict-transport-security"))
        .cloned()
        .collect();

    // `HeaderEdit::clone` here is two `Rc::clone`s on the shared
    // key/val plus a one-byte `mode` copy — no buffer allocation.
    if let Some(edit) = new_edit {
        headers_response.push(edit.clone());
    }

    Frontend {
        headers_response: headers_response.into(),
        // every other field is unchanged
        ..frontend.clone()
    }
}

/// Render an [`HstsConfig`] into a canonical RFC 6797 §6.1
/// `Strict-Transport-Security` header value: `max-age=N` first, then
/// optional `; includeSubDomains`, then optional `; preload`. No
/// trailing semicolon. `includeSubDomains` is the RFC §6.1 spelling
/// (camelCase); `preload` is lowercase per the de-facto Chrome/HSTS
/// preload-list convention (https://hstspreload.org/).
///
/// Returns `None` when the config has no `max_age` (the caller should
/// have substituted the default at config-load via
/// `command/src/config.rs::FileHstsConfig::to_proto` before reaching
/// this site; if it didn't, a `None` here suppresses the emission so a
/// malformed wire frame can't leak `max-age=0` accidentally).
pub fn render_hsts(cfg: &HstsConfig) -> Option<String> {
    let max_age = cfg.max_age?;
    let mut s = format!("max-age={max_age}");
    if matches!(cfg.include_subdomains, Some(true)) {
        s.push_str("; includeSubDomains");
    }
    if matches!(cfg.preload, Some(true)) {
        s.push_str("; preload");
    }
    Some(s)
}

/// A single header mutation collected from a [`Frontend`] configuration.
///
/// `key` and `val` are owned via [`Rc`] so a `Frontend` can be held by many
/// routing entries (pre, tree, post) without copying the underlying bytes.
///
/// `mode` controls how the per-stream `apply_response_header_edits` pass
/// emits the entry on the wire — see [`HeaderEditMode`]. Operator-supplied
/// `[[...frontends.headers]]` entries default to [`HeaderEditMode::Append`]
/// (preserving the legacy empty-val-deletes encoding); typed policies
/// (HSTS, future RFC-correct response policies) opt into
/// [`HeaderEditMode::SetIfAbsent`].
#[derive(Clone, PartialEq, Eq)]
pub struct HeaderEdit {
    pub key: Rc<[u8]>,
    pub val: Rc<[u8]>,
    pub mode: HeaderEditMode,
}

impl Debug for HeaderEdit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "({:?}, {:?}, {:?})",
            String::from_utf8_lossy(&self.key),
            String::from_utf8_lossy(&self.val),
            self.mode,
        ))
    }
}

/// A parsed segment of a rewrite template.
///
/// `Host(i)` references the `i`-th host capture: index 0 is the full
/// hostname; positive indices are regex or wildcard subgroups. `Path(i)`
/// references the `i`-th path capture: index 0 is the full path; positive
/// indices are regex groups or prefix tails. `String` holds a literal
/// segment between captures.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RewritePart {
    String(String),
    Host(usize),
    Path(usize),
}

/// A pre-parsed rewrite template, decomposed into [`RewritePart`]s.
///
/// `RewriteParts` is built once at frontend registration time
/// ([`Frontend::new`]) and then re-applied at lookup time via
/// [`RewriteParts::run`] against the captures collected by the router.
///
/// Grammar:
/// - `$HOST[N]` — substitute the N-th host capture
/// - `$PATH[N]` — substitute the N-th path capture
/// - any other byte sequence — substitute literally
///
/// Out-of-bounds capture indices substitute to the empty string at run
/// time, but [`RewriteParts::parse`] rejects templates that reference
/// capture indices the router cannot produce (the `*_cap_cap` arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteParts(Vec<RewritePart>);

impl RewriteParts {
    /// Parse `template` against the host/path capture caps the router can
    /// produce for the matching domain/path rule.
    ///
    /// `host_cap_cap` and `path_cap_cap` are upper bounds (exclusive) on the
    /// host and path capture indices the router can fill at lookup time.
    /// `used_index_host` / `used_index_path` are out parameters tracking the
    /// highest index actually referenced — callers use them to short-circuit
    /// capture extraction when no template references captures.
    ///
    /// Returns `None` on syntactically malformed templates: dangling `$`,
    /// missing closing `]`, non-digit index, or an index ≥ the cap.
    pub fn parse(
        template: &str,
        host_cap_cap: usize,
        path_cap_cap: usize,
        used_index_host: &mut usize,
        used_index_path: &mut usize,
    ) -> Option<Self> {
        let mut result = Vec::new();
        let mut i = 0;
        let pattern = template.as_bytes();
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
                let mut index = 0usize;
                let digits_start = i;
                while i < pattern.len() && pattern[i].is_ascii_digit() {
                    index = index
                        .checked_mul(10)?
                        .checked_add((pattern[i] - b'0') as usize)?;
                    i += 1;
                }
                if i == digits_start {
                    // no digits between the `[` and the `]`
                    return None;
                }
                if i >= pattern.len() || pattern[i] != b']' {
                    return None;
                }
                if is_host {
                    if index >= host_cap_cap {
                        return None;
                    }
                    if index >= *used_index_host {
                        *used_index_host = index + 1;
                    }
                    result.push(RewritePart::Host(index));
                } else {
                    if index >= path_cap_cap {
                        return None;
                    }
                    if index >= *used_index_path {
                        *used_index_path = index + 1;
                    }
                    result.push(RewritePart::Path(index));
                }
                i += 1; // consume `]`
            } else {
                let start = i;
                while i < pattern.len() && pattern[i] != b'$' {
                    i += 1;
                }
                // `pattern` is `template.as_bytes()` and the split is on
                // the ASCII byte `$` (0x24), which is always a single-byte
                // UTF-8 character — so `template[start..i]` lies on char
                // boundaries and is safe to index directly.
                result.push(RewritePart::String(template[start..i].to_owned()));
            }
        }
        // Every capture reference the parser emitted is within the caps it
        // was given; out-of-range indices return None above, never a part.
        debug_assert!(
            result.iter().all(|part| match part {
                RewritePart::Host(idx) => *idx < host_cap_cap,
                RewritePart::Path(idx) => *idx < path_cap_cap,
                RewritePart::String(_) => true,
            }),
            "a parsed rewrite template must only reference captures within the rule's caps",
        );
        debug_assert!(
            *used_index_host <= host_cap_cap && *used_index_path <= path_cap_cap,
            "the highest referenced capture index cannot exceed the cap",
        );
        Some(Self(result))
    }

    /// Substitute `host_captures` and `path_captures` into the template.
    ///
    /// Out-of-bounds captures substitute to an empty string. The result is
    /// allocated in one pass with the exact required capacity.
    pub fn run(&self, host_captures: &[&str], path_captures: &[&str]) -> String {
        let mut cap = 0usize;
        for part in &self.0 {
            cap += match part {
                RewritePart::String(s) => s.len(),
                RewritePart::Host(i) => host_captures.get(*i).map(|s| s.len()).unwrap_or(0),
                RewritePart::Path(i) => path_captures.get(*i).map(|s| s.len()).unwrap_or(0),
            };
        }
        let mut result = String::with_capacity(cap);
        for part in &self.0 {
            // String::write_str cannot fail — ignore the formatter result.
            let _ = match part {
                RewritePart::String(s) => result.write_str(s),
                RewritePart::Host(i) => result.write_str(host_captures.get(*i).unwrap_or(&"")),
                RewritePart::Path(i) => result.write_str(path_captures.get(*i).unwrap_or(&"")),
            };
        }
        // The capacity pass and the write pass consult the same parts and
        // captures, so the single up-front allocation must be exact — the
        // result never reallocates.
        debug_assert_eq!(
            result.len(),
            cap,
            "rewrite output length must equal the pre-computed one-pass capacity",
        );
        result
    }
}

/// What to do with the traffic for a routed frontend.
///
/// Built once at frontend registration time. The expensive work (parsing
/// rewrite templates, resolving headers into [`HeaderEdit`]s) happens here
/// so [`Router::lookup`] can run cheaply on the hot path.
///
/// A clusterless frontend with `redirect == FORWARD` is coerced to
/// `UNAUTHORIZED` in [`Frontend::new`] to avoid a forward loop with no
/// backend; the explicit `UNAUTHORIZED` policy then renders a 401.
///
/// Tags are wrapped in [`Rc<CachedTags>`] so the same frontend can be
/// referenced from multiple routing slots (pre/tree/post) without copying.
#[derive(Debug, Clone)]
pub struct Frontend {
    pub cluster_id: Option<ClusterId>,
    pub redirect: RedirectPolicy,
    pub redirect_scheme: RedirectScheme,
    pub redirect_template: Option<String>,
    /// Number of host captures the router will collect for this frontend.
    /// Sized from the matching [`DomainRule`]; the router skips capture
    /// extraction entirely when this is 0 (no rewrite references `$HOST[…]`).
    pub capture_cap_host: usize,
    /// Number of path captures the router will collect for this frontend.
    /// Sized from the matching [`PathRule`]; the router skips capture
    /// extraction entirely when this is 0 (no rewrite references `$PATH[…]`).
    pub capture_cap_path: usize,
    pub rewrite_host: Option<RewriteParts>,
    pub rewrite_path: Option<RewriteParts>,
    pub rewrite_port: Option<u16>,
    pub headers_request: Rc<[HeaderEdit]>,
    pub headers_response: Rc<[HeaderEdit]>,
    pub required_auth: bool,
    pub tags: Option<Rc<CachedTags>>,
    /// `true` when the materialised HSTS edit (if any) in
    /// [`Self::headers_response`] came from the listener-default
    /// `HttpsListenerConfig.hsts` rather than the per-frontend
    /// `RequestHttpFrontend.hsts` block. Consulted by
    /// [`Router::refresh_inheriting_hsts`] so a
    /// `UpdateHttpsListenerConfig.hsts` patch reflows the new default
    /// onto inheriting frontends without overwriting explicit
    /// per-frontend HSTS overrides.
    pub inherits_listener_hsts: bool,
}

/// Origin of the per-frontend HSTS policy carried by an
/// [`HttpFrontend`] when the router materialises it into a
/// [`Frontend`]. Tracked separately because the resolved
/// `HttpFrontend.hsts` field is the same shape regardless of how it
/// was filled in — the inheritance bit lets later listener-default
/// patches refresh inheriting frontends without disturbing explicit
/// per-frontend overrides.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HstsOrigin {
    /// `front.hsts` came from the per-frontend configuration directly
    /// (operator wrote `[clusters.<id>.frontends.hsts]` in TOML or
    /// passed `--hsts-*` on the CLI). Listener-default patches do NOT
    /// refresh this entry.
    Explicit,
    /// `front.hsts` was filled in by `add_https_frontend` from the
    /// listener-default `HttpsListenerConfig.hsts`. A future
    /// `UpdateHttpsListenerConfig.hsts` patch will refresh this entry
    /// via [`Router::refresh_inheriting_hsts`].
    InheritedFromListenerDefault,
}

impl PartialEq for Frontend {
    fn eq(&self, other: &Self) -> bool {
        // Frontend instances share the rest of their fields with the
        // originating HttpFrontend; equality is decided by the same fields
        // the router uses for de-duplication.
        self.cluster_id == other.cluster_id
            && self.redirect == other.redirect
            && self.redirect_scheme == other.redirect_scheme
            && self.redirect_template == other.redirect_template
            && self.rewrite_host == other.rewrite_host
            && self.rewrite_path == other.rewrite_path
            && self.rewrite_port == other.rewrite_port
            && self.headers_request == other.headers_request
            && self.headers_response == other.headers_response
            && self.required_auth == other.required_auth
    }
}

impl Eq for Frontend {}

impl std::hash::Hash for Frontend {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.cluster_id.hash(state);
        // RedirectPolicy / RedirectScheme are i32-backed proto enums; hash
        // them as i32 to avoid requiring a Hash impl on the generated enum.
        (self.redirect as i32).hash(state);
        (self.redirect_scheme as i32).hash(state);
        self.redirect_template.hash(state);
        self.required_auth.hash(state);
    }
}

impl PartialOrd for Frontend {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Frontend {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.cluster_id
            .cmp(&other.cluster_id)
            .then_with(|| (self.redirect as i32).cmp(&(other.redirect as i32)))
            .then_with(|| (self.redirect_scheme as i32).cmp(&(other.redirect_scheme as i32)))
            .then_with(|| self.redirect_template.cmp(&other.redirect_template))
            .then_with(|| self.required_auth.cmp(&other.required_auth))
    }
}

impl Frontend {
    /// Build a [`Frontend`] from a domain/path rule pair and an
    /// [`HttpFrontend`] configuration.
    ///
    /// The richer proto-level fields (`redirect`, `redirect_scheme`,
    /// `redirect_template`, `rewrite_*`, `headers`, `required_auth`) are
    /// not yet carried on `HttpFrontend`; until they are, this constructor
    /// takes them as explicit arguments so the data flow is testable
    /// today and the call sites in `add_http_front` only need a one-line
    /// update once the fields are plumbed through.
    ///
    /// Coercions:
    /// - `redirect == UNAUTHORIZED` zeroes out rewrite/headers/auth — the
    ///   request will be rejected with a 401 regardless.
    /// - `redirect == FORWARD` on a clusterless frontend (`cluster_id ==
    ///   None`) is coerced to `UNAUTHORIZED` (logged as a warning) to
    ///   avoid a forward loop with no backend.
    ///
    /// Returns [`RouterError::InvalidHostRewrite`] /
    /// [`RouterError::InvalidPathRewrite`] when a rewrite template fails to
    /// parse against the rule's capture caps.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain_rule: &DomainRule,
        path_rule: &PathRule,
        front: &HttpFrontend,
        redirect: RedirectPolicy,
        redirect_scheme: RedirectScheme,
        redirect_template: Option<String>,
        rewrite_host: Option<String>,
        rewrite_path: Option<String>,
        rewrite_port: Option<u16>,
        headers: &[sozu_command::proto::command::Header],
        required_auth: bool,
        hsts_origin: HstsOrigin,
    ) -> Result<Self, RouterError> {
        // HSTS is read from `front.hsts` directly inside the function;
        // an explicit parameter would be redundant since `front` is
        // already in scope and the field is the single source of truth.
        // The `hsts_origin` parameter records *where* `front.hsts` came
        // from so [`Router::refresh_inheriting_hsts`] can reflow listener
        // defaults without disturbing explicit per-frontend overrides.
        let hsts = front.hsts.as_ref();
        let inherits_listener_hsts =
            matches!(hsts_origin, HstsOrigin::InheritedFromListenerDefault) && hsts.is_some();
        let cluster_id = front.cluster_id.clone();
        let tags = front
            .tags
            .clone()
            .map(|tags| Rc::new(CachedTags::new(tags)));

        // Coerce clusterless FORWARD to UNAUTHORIZED before doing any
        // expensive parsing: those routes can never proceed to a backend, so
        // emitting a 401 is the safe default. Empty redirect_template is
        // treated as None semantics so we never store an empty Rc<[…]>.
        let redirect_template = redirect_template.filter(|s| !s.is_empty());
        let rewrite_host = rewrite_host.filter(|s| !s.is_empty());
        let rewrite_path = rewrite_path.filter(|s| !s.is_empty());

        let deny = match (&cluster_id, redirect) {
            (_, RedirectPolicy::Unauthorized) => true,
            (None, RedirectPolicy::Forward) => {
                let (domain_kind, domain_bytes) = match &domain_rule {
                    DomainRule::Any => ("any", 0),
                    DomainRule::Exact(value) => ("exact", value.len()),
                    DomainRule::Wildcard(value) => ("wildcard", value.len()),
                    DomainRule::Regex(value) => ("regex", value.as_str().len()),
                };
                let (path_kind, path_bytes) = match &path_rule {
                    PathRule::Prefix(value) => ("prefix", value.len()),
                    PathRule::Regex(value) => ("regex", value.as_str().len()),
                    PathRule::Equals(value) => ("equals", value.len()),
                };
                warn!(
                    "{} Frontend[domain_kind={}, domain_bytes={}, path_kind={}, path_bytes={}]: forward on clusterless frontends are unauthorized",
                    log_module_context!(),
                    domain_kind,
                    domain_bytes,
                    path_kind,
                    path_bytes,
                );
                true
            }
            _ => false,
        };
        if deny {
            // The Unauthorized policy zeroes out request rewrites and
            // header injections (the request never reaches a backend),
            // but RFC 6797 §8.1 still requires HSTS on the 401 default
            // answer when the frontend is on an HTTPS listener. Build
            // the HSTS edit (when configured) so the per-stream
            // snapshot copy in `mux/router.rs` carries it through to
            // `set_default_answer_with_retry_after`.
            let mut deny_headers_response: Vec<HeaderEdit> = Vec::new();
            if let Some(cfg) = hsts
                && matches!(cfg.enabled, Some(true))
                && let Some(rendered) = render_hsts(cfg)
            {
                let mode = if matches!(cfg.force_replace_backend, Some(true)) {
                    HeaderEditMode::Set
                } else {
                    HeaderEditMode::SetIfAbsent
                };
                deny_headers_response.push(HeaderEdit {
                    key: Rc::from(&b"strict-transport-security"[..]),
                    val: rendered.into_bytes().into(),
                    mode,
                });
                crate::incr!(names::http::HSTS_FRONTEND_ADDED);
            }

            return Ok(Self {
                cluster_id,
                redirect: RedirectPolicy::Unauthorized,
                redirect_scheme,
                redirect_template: None,
                capture_cap_host: 0,
                capture_cap_path: 0,
                rewrite_host: None,
                rewrite_path: None,
                rewrite_port: None,
                headers_request: Rc::new([]),
                headers_response: deny_headers_response.into(),
                required_auth,
                tags,
                inherits_listener_hsts,
            });
        }

        // Capture caps: the maximum index a `$HOST[N]` / `$PATH[N]`
        // template can reference for this rule pair. Index 0 is always the
        // full hostname/path; subsequent indices are wildcard tail / regex
        // groups.
        let mut capture_cap_host = match domain_rule {
            DomainRule::Any => 1,
            DomainRule::Exact(_) => 1,
            DomainRule::Wildcard(_) => 2,
            DomainRule::Regex(regex) => regex.captures_len(),
        };
        let mut capture_cap_path = match path_rule {
            PathRule::Equals(_) => 1,
            PathRule::Prefix(_) => 2,
            PathRule::Regex(regex) => regex.captures_len(),
        };
        let mut used_capture_host = 0usize;
        let mut used_capture_path = 0usize;
        let rewrite_host_parts = if let Some(p) = rewrite_host {
            Some(
                RewriteParts::parse(
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
        let rewrite_path_parts = if let Some(p) = rewrite_path {
            Some(
                RewriteParts::parse(
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
        // Skip capture extraction at lookup time when no template references
        // a capture for this dimension.
        if used_capture_host == 0 {
            capture_cap_host = 0;
        }
        if used_capture_path == 0 {
            capture_cap_path = 0;
        }

        let mut headers_request = Vec::new();
        let mut headers_response = Vec::new();
        for header in headers {
            let edit = HeaderEdit {
                key: header.key.as_bytes().into(),
                val: header.val.as_bytes().into(),
                mode: HeaderEditMode::Append,
            };
            match header.position() {
                HeaderPosition::Request => headers_request.push(edit),
                HeaderPosition::Response => headers_response.push(edit),
                HeaderPosition::Both => {
                    headers_request.push(edit.clone());
                    headers_response.push(edit);
                }
                // The proto-default-encoded shape (`position: 0`). The TOML
                // loader rejects this case in `parse_header_edit` so the
                // path is only reachable via a manually-constructed
                // `Header { position: 0, … }` from a buggy or older
                // client. Drop the edit rather than guessing a position.
                HeaderPosition::Unspecified => {
                    warn!(
                        "{} dropping {:?} with HEADER_POSITION_UNSPECIFIED",
                        log_module_context!(),
                        header,
                    );
                }
            }
        }

        // Materialise HSTS (RFC 6797) into the response-side header
        // collection as a single `SetIfAbsent` edit so an upstream-emitted
        // `Strict-Transport-Security` survives unchanged (RFC 6797 §6.1
        // single-header requirement). `enabled = Some(false)` is the
        // explicit-disable signal — emit nothing. The §7.2 "no STS over
        // plaintext HTTP" gate is enforced by the runtime snapshot site,
        // which only copies `headers_response` for HTTPS-served requests.
        if let Some(cfg) = hsts
            && matches!(cfg.enabled, Some(true))
        {
            if let Some(rendered) = render_hsts(cfg) {
                // RFC 6797 §6.1 default: PRESERVE backend-supplied STS
                // (SetIfAbsent). Operator opts into harden-centrally
                // override via `force_replace_backend = true`, which
                // selects `Set` (delete-then-insert) so any backend
                // STS is replaced with sozu's rendered policy.
                let mode = if matches!(cfg.force_replace_backend, Some(true)) {
                    HeaderEditMode::Set
                } else {
                    HeaderEditMode::SetIfAbsent
                };
                headers_response.push(HeaderEdit {
                    key: Rc::from(&b"strict-transport-security"[..]),
                    val: rendered.into_bytes().into(),
                    mode,
                });
                crate::incr!(names::http::HSTS_FRONTEND_ADDED);
            } else {
                // Both upstream config layers (FileHstsConfig::to_proto and
                // build_hsts_from_cli) substitute DEFAULT_HSTS_MAX_AGE when
                // enabled = Some(true) && max_age = None, so reaching this
                // branch means a programmatic IPC sender produced an
                // ill-formed HstsConfig. Surface it loudly rather than
                // silently emitting no header — operators inspecting their
                // dashboards for http.hsts.unrendered will catch the bug.
                warn!(
                    "{} HSTS enabled = true on frontend cluster_id_bytes={:?} but render_hsts \
                     returned None (max_age missing). Frontend will not emit \
                     Strict-Transport-Security; the config layer that built \
                     this HstsConfig must substitute DEFAULT_HSTS_MAX_AGE.",
                    log_module_context!(),
                    cluster_id.as_ref().map(String::len),
                );
                crate::incr!(names::http::HSTS_UNRENDERED);
            }
        }

        Ok(Frontend {
            cluster_id,
            redirect,
            redirect_scheme,
            redirect_template,
            capture_cap_host,
            capture_cap_path,
            rewrite_host: rewrite_host_parts,
            rewrite_path: rewrite_path_parts,
            rewrite_port,
            headers_request: headers_request.into(),
            headers_response: headers_response.into(),
            required_auth,
            tags,
            inherits_listener_hsts,
        })
    }

    /// Build a minimal Frontend that simply forwards to `cluster_id`,
    /// with no rewrite / header / auth configuration. Equivalent to a
    /// `Route::ClusterId(cluster_id)` lookup-wise (lookup short-circuits
    /// to `RouteResult::forward(id)` for both shapes), with the added
    /// ability to carry response-header edits — used by
    /// [`Router::refresh_inheriting_hsts`] to promote a lightweight
    /// `Route::ClusterId` to a `Route::Frontend` carrying just the
    /// listener-default HSTS edit. The `inherits_listener_hsts = true`
    /// marker lets subsequent listener-default patches keep refreshing
    /// the promoted entry.
    pub(crate) fn minimal_forward(cluster_id: ClusterId) -> Self {
        Self {
            cluster_id: Some(cluster_id),
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
            required_auth: false,
            tags: None,
            inherits_listener_hsts: true,
        }
    }

    /// Build a minimal clusterless Frontend with the
    /// [`RedirectPolicy::Unauthorized`] policy. Equivalent to a
    /// `Route::Deny` lookup-wise (both yield a 401 default answer),
    /// with the added ability to carry response-header edits — used by
    /// [`Router::refresh_inheriting_hsts`] to promote `Route::Deny`
    /// entries when the listener-default HSTS becomes enabled, so the
    /// 401 default answer carries the `Strict-Transport-Security`
    /// header per RFC 6797 §8.1. The `inherits_listener_hsts = true`
    /// marker lets subsequent listener-default patches keep refreshing
    /// the promoted entry.
    pub(crate) fn minimal_deny() -> Self {
        Self {
            cluster_id: None,
            redirect: RedirectPolicy::Unauthorized,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::new([]),
            required_auth: false,
            tags: None,
            inherits_listener_hsts: true,
        }
    }
}

/// Routing decision returned by [`Router::lookup`] and consumed by the
/// session layer.
///
/// Computed from a matched [`Frontend`] by running [`RewriteParts::run`]
/// against the captures collected during routing. Legacy [`Route::ClusterId`]
/// and [`Route::Deny`] entries synthesize a minimal `RouteResult` with the
/// proto enums set to defaults (`FORWARD` / `UNAUTHORIZED`) so existing
/// session code keeps working until the mux layer is updated to read every
/// `RouteResult` field directly.
///
/// Implements `PartialEq` for test parity: existing router tests compare
/// `router.lookup(...)` against an expected route. Equality compares every
/// public field, including the `Rc<[HeaderEdit]>` slices via pointer-or-content
/// equality on the slice contents.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteResult {
    pub cluster_id: Option<ClusterId>,
    pub redirect: RedirectPolicy,
    pub redirect_scheme: RedirectScheme,
    pub redirect_template: Option<String>,
    pub rewritten_host: Option<String>,
    pub rewritten_path: Option<String>,
    pub rewritten_port: Option<u16>,
    pub headers_request: Rc<[HeaderEdit]>,
    pub headers_response: Rc<[HeaderEdit]>,
    pub required_auth: bool,
    pub tags: Option<Rc<CachedTags>>,
}

impl RouteResult {
    /// Synthesize a `RouteResult` representing a 401 (Deny) decision.
    pub fn deny(cluster_id: Option<ClusterId>) -> Self {
        Self {
            cluster_id,
            redirect: RedirectPolicy::Unauthorized,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            rewritten_host: None,
            rewritten_path: None,
            rewritten_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::new([]),
            required_auth: false,
            tags: None,
        }
    }

    /// Synthesize a `RouteResult` representing a "forward to this cluster"
    /// decision (legacy [`Route::ClusterId`] adapter).
    pub fn forward(cluster_id: ClusterId) -> Self {
        Self {
            cluster_id: Some(cluster_id),
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            rewritten_host: None,
            rewritten_path: None,
            rewritten_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::new([]),
            required_auth: false,
            tags: None,
        }
    }

    /// Build a `RouteResult` from a [`Frontend`] and the captures collected
    /// for this lookup.
    fn from_frontend(
        frontend: &Frontend,
        captures_host: Vec<&str>,
        path: &[u8],
        path_rule: &PathRule,
    ) -> Self {
        // Unauthorized short-circuit: skip the path-capture extraction
        // entirely — the response will not consume any rewrite output.
        // `headers_response` IS preserved here (unlike `headers_request`)
        // so the per-stream snapshot copy in `mux/router.rs` can still
        // pick up the per-frontend HSTS edit and inject it on the 401
        // default answer (RFC 6797 §8.1 — HSTS applies to all response
        // codes, including the proxy's typed unauthorized answer).
        if frontend.redirect == RedirectPolicy::Unauthorized {
            return Self {
                cluster_id: frontend.cluster_id.clone(),
                redirect: RedirectPolicy::Unauthorized,
                redirect_scheme: frontend.redirect_scheme,
                redirect_template: frontend.redirect_template.clone(),
                rewritten_host: None,
                rewritten_path: None,
                rewritten_port: None,
                headers_request: Rc::new([]),
                headers_response: frontend.headers_response.clone(),
                required_auth: frontend.required_auth,
                tags: frontend.tags.clone(),
            };
        }

        let mut captures_path: Vec<&str> = Vec::with_capacity(frontend.capture_cap_path);
        if frontend.capture_cap_path > 0 {
            captures_path.push(from_utf8(path).unwrap_or_default());
            match path_rule {
                PathRule::Prefix(prefix) => {
                    let tail_start = prefix.len().min(path.len());
                    captures_path.push(from_utf8(&path[tail_start..]).unwrap_or_default());
                }
                PathRule::Regex(regex) => {
                    if let Some(caps) = regex.captures(path) {
                        captures_path.extend(caps.iter().skip(1).map(|c| {
                            c.map(|m| from_utf8(m.as_bytes()).unwrap_or_default())
                                .unwrap_or("")
                        }));
                    }
                }
                PathRule::Equals(_) => {}
            }
        }

        Self {
            cluster_id: frontend.cluster_id.clone(),
            redirect: frontend.redirect,
            redirect_scheme: frontend.redirect_scheme,
            redirect_template: frontend.redirect_template.clone(),
            rewritten_host: frontend
                .rewrite_host
                .as_ref()
                .map(|rewrite| rewrite.run(&captures_host, &captures_path)),
            rewritten_path: frontend
                .rewrite_path
                .as_ref()
                .map(|rewrite| rewrite.run(&captures_host, &captures_path)),
            rewritten_port: frontend.rewrite_port,
            headers_request: frontend.headers_request.clone(),
            headers_response: frontend.headers_response.clone(),
            required_auth: frontend.required_auth,
            tags: frontend.tags.clone(),
        }
    }

    /// Build a `RouteResult` for a pre/post rule match.
    ///
    /// Pre/post rules carry the matched [`DomainRule`] directly so we can
    /// extract host captures from it without going through the trie.
    fn new_no_trie<'a>(
        domain: &'a [u8],
        domain_rule: &DomainRule,
        path: &'a [u8],
        path_rule: &PathRule,
        route: &Route,
    ) -> Self {
        let frontend = match route {
            Route::Frontend(f) => f.clone(),
            Route::ClusterId(id) => return Self::forward(id.clone()),
            Route::Deny => return Self::deny(None),
        };
        let mut captures_host: Vec<&str> = Vec::with_capacity(frontend.capture_cap_host);
        if frontend.capture_cap_host > 0 {
            captures_host.push(from_utf8(domain).unwrap_or_default());
            match domain_rule {
                DomainRule::Wildcard(suffix) => {
                    let head_end = domain.len().saturating_sub(suffix.len().saturating_sub(1));
                    captures_host.push(from_utf8(&domain[..head_end]).unwrap_or_default());
                }
                DomainRule::Regex(regex) => {
                    if let Some(caps) = regex.captures(domain) {
                        captures_host.extend(caps.iter().skip(1).map(|c| {
                            c.map(|m| from_utf8(m.as_bytes()).unwrap_or_default())
                                .unwrap_or("")
                        }));
                    }
                }
                DomainRule::Any | DomainRule::Exact(_) => {}
            }
        }
        Self::from_frontend(&frontend, captures_host, path, path_rule)
    }

    /// Build a `RouteResult` for a tree-match.
    ///
    /// Tree matches carry the captures collected by the trie traversal
    /// (`TrieMatches`) alongside the matched leaf path rule.
    fn new_with_trie<'a, 'b>(
        domain: &'a [u8],
        domain_submatches: TrieMatches<'a, 'b>,
        path: &'a [u8],
        path_rule: &PathRule,
        route: &Route,
    ) -> Self {
        let frontend = match route {
            Route::Frontend(f) => f.clone(),
            Route::ClusterId(id) => return Self::forward(id.clone()),
            Route::Deny => return Self::deny(None),
        };
        let mut captures_host: Vec<&str> = Vec::with_capacity(frontend.capture_cap_host);
        if frontend.capture_cap_host > 0 {
            captures_host.push(from_utf8(domain).unwrap_or_default());
            for submatch in &domain_submatches {
                match submatch {
                    TrieSubMatch::Wildcard(part) => {
                        captures_host.push(from_utf8(part).unwrap_or_default());
                    }
                    TrieSubMatch::Regexp(part, regex) => {
                        if let Some(caps) = regex.captures(part) {
                            captures_host.extend(caps.iter().skip(1).map(|c| {
                                c.map(|m| from_utf8(m.as_bytes()).unwrap_or_default())
                                    .unwrap_or("")
                            }));
                        }
                    }
                }
            }
        }
        Self::from_frontend(&frontend, captures_host, path, path_rule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_http_frontend() -> HttpFrontend {
        HttpFrontend {
            cluster_id: Some("cluster".to_owned()),
            address: "127.0.0.1:8080"
                .parse()
                .expect("test frontend address must parse"),
            hostname: "example.com".to_owned(),
            path: CommandPathRule::prefix("/".to_owned()),
            method: None,
            position: RulePosition::Tree,
            tags: None,
            redirect: None,
            redirect_scheme: None,
            redirect_template: None,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            required_auth: None,
            headers: Vec::new(),
            hsts: None,
        }
    }

    #[test]
    fn clusterless_forward_warning_redacts_domain_and_path_rules() {
        const DOMAIN_SECRET: &str = "clusterless_domain_secret_sentinel";
        const PATH_SECRET: &str = "CLUSTERLESS_PATH_SECRET_SENTINEL";

        // The hostname stays below `MAX_HOSTNAME_LENGTH` so it reaches the
        // code under test instead of the pre-parse bound; the redaction
        // property being asserted is length-independent.
        let domain = format!("{DOMAIN_SECRET}{}", "x".repeat(2048));
        let path = format!("/{PATH_SECRET}{}", "x".repeat(4096));
        let domain_len = domain.len();
        let path_len = path.len();
        let output = crate::capture_test_logs(move || {
            let mut router = Router::new();
            let mut front = test_http_frontend();
            front.cluster_id = None;
            front.hostname = domain;
            front.path = CommandPathRule::prefix(path);
            front.redirect = Some(RedirectPolicy::Forward as i32);
            router
                .add_http_front(&front)
                .expect("clusterless forward must be coerced to unauthorized");
        });

        for secret in [DOMAIN_SECRET, PATH_SECRET] {
            assert!(
                !output.contains(secret),
                "clusterless-forward warning leaked rule marker {secret}"
            );
        }
        for metadata in [
            "domain_kind=exact".to_owned(),
            format!("domain_bytes={domain_len}"),
            "path_kind=prefix".to_owned(),
            format!("path_bytes={path_len}"),
        ] {
            assert!(
                output.contains(&metadata),
                "clusterless-forward warning omitted bounded metadata {metadata}: {output}"
            );
        }
        assert!(
            output.len() <= 512,
            "clusterless-forward warning is not bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn malformed_hsts_warning_redacts_cluster_id() {
        const CLUSTER_SECRET: &str = "MALFORMED_HSTS_CLUSTER_SECRET_SENTINEL";

        let cluster_id = format!("{CLUSTER_SECRET}{}", "x".repeat(4096));
        let cluster_id_len = cluster_id.len();
        let output = crate::capture_test_logs(move || {
            let mut router = Router::new();
            let mut front = test_http_frontend();
            front.cluster_id = Some(cluster_id);
            front.hsts = Some(HstsConfig {
                enabled: Some(true),
                max_age: None,
                include_subdomains: Some(true),
                preload: Some(true),
                force_replace_backend: Some(false),
            });
            router
                .add_http_front(&front)
                .expect("malformed HSTS must preserve routing and omit the header");
        });

        assert!(
            !output.contains(CLUSTER_SECRET),
            "malformed-HSTS warning leaked cluster id {CLUSTER_SECRET}"
        );
        assert!(
            output.contains(&format!("cluster_id_bytes=Some({cluster_id_len})")),
            "malformed-HSTS warning omitted the bounded cluster id length: {output}"
        );
        assert!(
            output.len() <= 768,
            "malformed-HSTS warning is not bounded: {} bytes",
            output.len()
        );
    }

    #[test]
    fn router_errors_redact_frontend_rule_and_rewrite_fields() {
        const PATH_SECRET: &str = "ROUTER_ERROR_PATH_SECRET_SENTINEL";
        const HOSTNAME_SECRET: &str = "ROUTER_ERROR_HOSTNAME_SECRET_SENTINEL";
        const HOST_REWRITE_SECRET: &str = "ROUTER_ERROR_HOST_REWRITE_SECRET_SENTINEL";
        const PATH_REWRITE_SECRET: &str = "ROUTER_ERROR_PATH_REWRITE_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let cases = [
            (
                "path_bytes",
                PATH_SECRET,
                long_value(PATH_SECRET).len(),
                RouterError::InvalidPathRule(long_value(PATH_SECRET)),
            ),
            (
                "hostname_bytes",
                HOSTNAME_SECRET,
                long_value(HOSTNAME_SECRET).len(),
                RouterError::InvalidDomain {
                    hostname: long_value(HOSTNAME_SECRET),
                },
            ),
            (
                "rewrite_host_bytes",
                HOST_REWRITE_SECRET,
                long_value(HOST_REWRITE_SECRET).len(),
                RouterError::InvalidHostRewrite(long_value(HOST_REWRITE_SECRET)),
            ),
            (
                "rewrite_path_bytes",
                PATH_REWRITE_SECRET,
                long_value(PATH_REWRITE_SECRET).len(),
                RouterError::InvalidPathRewrite(long_value(PATH_REWRITE_SECRET)),
            ),
        ];

        for (length_label, secret, value_len, error) in cases {
            for (format_label, output) in [
                ("Display", error.to_string()),
                ("Debug", format!("{error:?}")),
            ] {
                assert!(
                    !output.contains(secret),
                    "RouterError {format_label} leaked frontend marker {secret}"
                );
                let metadata = format!("{length_label}={value_len}");
                assert!(
                    output.contains(&metadata),
                    "RouterError {format_label} omitted bounded metadata {metadata}: {output}"
                );
                assert!(
                    output.len() <= 256,
                    "RouterError {format_label} output is not bounded: {} bytes",
                    output.len()
                );
            }
        }
    }

    #[test]
    fn route_miss_error_retains_inputs_but_bounds_textual_formatting() {
        const HOST_SECRET: &str = "ROUTE_MISS_HOST_SECRET_SENTINEL";
        const PATH_SECRET: &str = "ROUTE_MISS_PATH_SECRET_SENTINEL";
        const METHOD_SECRET: &str = "ROUTE_MISS_METHOD_SECRET_SENTINEL";

        let long_value = |marker: &str| format!("{marker}{}", "x".repeat(4096));
        let host = long_value(HOST_SECRET);
        let path = long_value(PATH_SECRET);
        let method = Method::Custom(long_value(METHOD_SECRET));
        let error = match Router::new().lookup(&host, &path, &method) {
            Err(error) => error,
            Ok(_) => panic!("empty router must return a route miss"),
        };

        match &error {
            RouterError::RouteNotFound {
                host: retained_host,
                path: retained_path,
                method: retained_method,
            } => {
                assert_eq!(retained_host, &host);
                assert_eq!(retained_path, &path);
                assert_eq!(retained_method, &method);
            }
            other => panic!("expected RouterError::RouteNotFound, got {other:?}"),
        }

        for output in [error.to_string(), format!("{error:?}")] {
            for secret in [HOST_SECRET, PATH_SECRET, METHOD_SECRET] {
                assert!(
                    !output.contains(secret),
                    "route miss formatting leaked {secret}: {output}"
                );
            }
            for metadata in [
                format!("host_bytes={}", host.len()),
                format!("path_bytes={}", path.len()),
                format!("bytes={}", method.as_ref().len()),
            ] {
                assert!(
                    output.contains(&metadata),
                    "route miss formatting omitted {metadata}: {output}"
                );
            }
            assert!(
                output.len() <= 256,
                "route miss formatting is not bounded: {} bytes",
                output.len()
            );
        }
    }

    #[test]
    fn render_hsts_max_age_only() {
        let cfg = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        };
        assert_eq!(render_hsts(&cfg), Some("max-age=31536000".to_owned()));
    }

    #[test]
    fn render_hsts_with_include_subdomains() {
        let cfg = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        assert_eq!(
            render_hsts(&cfg),
            Some("max-age=31536000; includeSubDomains".to_owned())
        );
    }

    #[test]
    fn render_hsts_with_preload_only() {
        let cfg = HstsConfig {
            enabled: Some(true),
            max_age: Some(63_072_000),
            include_subdomains: None,
            preload: Some(true),
            force_replace_backend: None,
        };
        assert_eq!(
            render_hsts(&cfg),
            Some("max-age=63072000; preload".to_owned())
        );
    }

    #[test]
    fn render_hsts_full() {
        let cfg = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: Some(true),
            preload: Some(true),
            force_replace_backend: None,
        };
        assert_eq!(
            render_hsts(&cfg),
            Some("max-age=31536000; includeSubDomains; preload".to_owned())
        );
    }

    #[test]
    fn render_hsts_kill_switch_max_age_zero() {
        let cfg = HstsConfig {
            enabled: Some(true),
            max_age: Some(0),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        // `max_age = 0` is the RFC 6797 §11.4 kill switch and renders
        // verbatim — UA receives it and stops treating the host as a
        // Known HSTS Host.
        assert_eq!(
            render_hsts(&cfg),
            Some("max-age=0; includeSubDomains".to_owned())
        );
    }

    #[test]
    fn render_hsts_omitted_when_max_age_missing() {
        let cfg = HstsConfig {
            enabled: Some(true),
            max_age: None,
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        // The TOML loader substitutes the default at config-load; if the
        // field reaches `render_hsts` as `None`, suppress emission so a
        // malformed wire frame can't accidentally render `max-age=`.
        assert_eq!(render_hsts(&cfg), None);
    }

    #[test]
    fn rebuild_with_listener_hsts_replaces_existing_entry() {
        // An inheriting frontend whose listener-default HSTS changed
        // from 1y → 2y must end up with the 2y entry on its
        // headers_response, with no leftover 1y entry.
        let frontend = Frontend {
            cluster_id: Some("api".to_owned()),
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::from(vec![
                HeaderEdit {
                    key: Rc::from(&b"x-cache"[..]),
                    val: Rc::from(&b"hit"[..]),
                    mode: HeaderEditMode::Append,
                },
                HeaderEdit {
                    key: Rc::from(&b"strict-transport-security"[..]),
                    val: Rc::from(&b"max-age=31536000"[..]),
                    mode: HeaderEditMode::SetIfAbsent,
                },
            ]),
            required_auth: false,
            tags: None,
            inherits_listener_hsts: true,
        };
        let new_hsts = HstsConfig {
            enabled: Some(true),
            max_age: Some(63_072_000),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        let new_edit = build_listener_hsts_edit(Some(&new_hsts));
        let rebuilt = rebuild_with_listener_hsts(&frontend, new_edit.as_ref());

        let response: Vec<_> = rebuilt.headers_response.iter().collect();
        assert_eq!(response.len(), 2, "x-cache + new STS, no leftover STS");
        assert_eq!(&*response[0].key, b"x-cache");
        assert_eq!(&*response[1].key, b"strict-transport-security");
        assert_eq!(
            &*response[1].val,
            b"max-age=63072000; includeSubDomains".as_slice()
        );
        assert!(rebuilt.inherits_listener_hsts);
    }

    #[test]
    fn rebuild_with_listener_hsts_strips_when_none() {
        // Listener-default HSTS removed → strip the existing STS edit
        // and add nothing. Operator response headers stay in place.
        let frontend = Frontend {
            cluster_id: Some("api".to_owned()),
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::from(vec![
                HeaderEdit {
                    key: Rc::from(&b"x-cache"[..]),
                    val: Rc::from(&b"hit"[..]),
                    mode: HeaderEditMode::Append,
                },
                HeaderEdit {
                    key: Rc::from(&b"strict-transport-security"[..]),
                    val: Rc::from(&b"max-age=31536000"[..]),
                    mode: HeaderEditMode::SetIfAbsent,
                },
            ]),
            required_auth: false,
            tags: None,
            inherits_listener_hsts: true,
        };
        let new_edit = build_listener_hsts_edit(None);
        let rebuilt = rebuild_with_listener_hsts(&frontend, new_edit.as_ref());
        let response: Vec<_> = rebuilt.headers_response.iter().collect();
        assert_eq!(response.len(), 1);
        assert_eq!(&*response[0].key, b"x-cache");
    }

    #[test]
    fn rebuild_with_listener_hsts_disabled_strips() {
        // `enabled = Some(false)` is the explicit-disable signal; the
        // existing STS entry is dropped and no new one is added.
        let frontend = Frontend {
            cluster_id: Some("api".to_owned()),
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::from(vec![HeaderEdit {
                key: Rc::from(&b"strict-transport-security"[..]),
                val: Rc::from(&b"max-age=31536000"[..]),
                mode: HeaderEditMode::SetIfAbsent,
            }]),
            required_auth: false,
            tags: None,
            inherits_listener_hsts: true,
        };
        let new_hsts = HstsConfig {
            enabled: Some(false),
            max_age: None,
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        };
        let new_edit = build_listener_hsts_edit(Some(&new_hsts));
        let rebuilt = rebuild_with_listener_hsts(&frontend, new_edit.as_ref());
        assert_eq!(rebuilt.headers_response.len(), 0);
    }

    #[test]
    fn refresh_inheriting_hsts_skips_explicit_overrides() {
        // Two frontends: one inheriting (gets refreshed), one explicit
        // override (must NOT change). `Router::refresh_inheriting_hsts`
        // returns the count of refreshed entries.
        use crate::router::pattern_trie::TrieNode;
        let mut router = Router {
            pre: Vec::new(),
            tree: TrieNode::root(),
            post: Vec::new(),
        };
        let inheriting = Frontend {
            cluster_id: Some("api".to_owned()),
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::from(vec![HeaderEdit {
                key: Rc::from(&b"strict-transport-security"[..]),
                val: Rc::from(&b"max-age=31536000"[..]),
                mode: HeaderEditMode::SetIfAbsent,
            }]),
            required_auth: false,
            tags: None,
            inherits_listener_hsts: true,
        };
        let explicit = Frontend {
            cluster_id: Some("legacy".to_owned()),
            redirect: RedirectPolicy::Forward,
            redirect_scheme: RedirectScheme::UseSame,
            redirect_template: None,
            capture_cap_host: 0,
            capture_cap_path: 0,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            headers_request: Rc::new([]),
            headers_response: Rc::from(vec![HeaderEdit {
                key: Rc::from(&b"strict-transport-security"[..]),
                val: Rc::from(&b"max-age=300"[..]),
                mode: HeaderEditMode::SetIfAbsent,
            }]),
            required_auth: false,
            tags: None,
            inherits_listener_hsts: false,
        };
        router.pre.push((
            DomainRule::Any,
            PathRule::Prefix("/api".to_owned()),
            MethodRule::new(None),
            Route::Frontend(Rc::new(inheriting)),
        ));
        router.post.push((
            DomainRule::Any,
            PathRule::Prefix("/legacy".to_owned()),
            MethodRule::new(None),
            Route::Frontend(Rc::new(explicit)),
        ));

        let new_hsts = HstsConfig {
            enabled: Some(true),
            max_age: Some(63_072_000),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        let count = router.refresh_inheriting_hsts(Some(&new_hsts));
        assert_eq!(count, 1, "only the inheriting frontend should refresh");

        if let Route::Frontend(rc) = &router.pre[0].3 {
            let response: Vec<_> = rc.headers_response.iter().collect();
            assert_eq!(
                &*response.last().unwrap().val,
                b"max-age=63072000; includeSubDomains".as_slice(),
                "inheriting frontend's STS must reflect the new listener default"
            );
        } else {
            panic!("pre[0] should be Route::Frontend");
        }
        if let Route::Frontend(rc) = &router.post[0].3 {
            let response: Vec<_> = rc.headers_response.iter().collect();
            assert_eq!(
                &*response.last().unwrap().val,
                b"max-age=300".as_slice(),
                "explicit override must be preserved unchanged"
            );
        } else {
            panic!("post[0] should be Route::Frontend");
        }
    }

    #[test]
    fn refresh_inheriting_hsts_promotes_clusterid_on_enable() {
        // The "no policy" frontend case observed on cleverapps.io shared
        // (91k+ frontends, 99 % stored as `Route::ClusterId` with no
        // policy fields). Before the fix, `refresh_inheriting_hsts`
        // walked only `Route::Frontend` entries and silently skipped
        // these — leaving HSTS unapplied across the entire fleet.
        // Now the lightweight route is promoted in place to a
        // `Route::Frontend` carrying just the HSTS edit; subsequent
        // patches refresh the promoted entry through the normal
        // `inherits_listener_hsts == true` path.
        use crate::router::pattern_trie::TrieNode;
        let mut router = Router {
            pre: Vec::new(),
            tree: TrieNode::root(),
            post: vec![(
                DomainRule::Any,
                PathRule::Prefix("/".to_owned()),
                MethodRule::new(None),
                Route::ClusterId("api".to_owned()),
            )],
        };

        let new_hsts = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        let count = router.refresh_inheriting_hsts(Some(&new_hsts));
        assert_eq!(count, 1, "the ClusterId entry must be promoted + counted");

        let Route::Frontend(rc) = &router.post[0].3 else {
            panic!("post[0] should now be Route::Frontend, not the original Route::ClusterId");
        };
        assert_eq!(rc.cluster_id.as_deref(), Some("api"));
        assert_eq!(
            rc.redirect,
            RedirectPolicy::Forward,
            "promoted entry must keep Forward semantics so lookup yields the same backend"
        );
        assert!(
            rc.inherits_listener_hsts,
            "promoted entry must mark itself inheriting so the next patch refreshes it"
        );
        let response: Vec<_> = rc.headers_response.iter().collect();
        assert_eq!(
            response.len(),
            1,
            "promoted entry carries exactly one STS edit, no operator headers"
        );
        assert_eq!(&*response[0].key, b"strict-transport-security");
        assert_eq!(
            &*response[0].val,
            b"max-age=31536000; includeSubDomains".as_slice()
        );
    }

    #[test]
    fn refresh_inheriting_hsts_promotes_deny_on_enable() {
        // RFC 6797 §8.1: HSTS applies to ALL HTTPS responses, including
        // proxy-generated 401s. A `Route::Deny` with no policy field at
        // add-time would, before the fix, never get HSTS injected onto
        // the 401 default answer even when the listener default
        // declared HSTS.
        use crate::router::pattern_trie::TrieNode;
        let mut router = Router {
            pre: Vec::new(),
            tree: TrieNode::root(),
            post: vec![(
                DomainRule::Any,
                PathRule::Prefix("/forbidden".to_owned()),
                MethodRule::new(None),
                Route::Deny,
            )],
        };

        let new_hsts = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        };
        let count = router.refresh_inheriting_hsts(Some(&new_hsts));
        assert_eq!(count, 1);

        let Route::Frontend(rc) = &router.post[0].3 else {
            panic!("post[0] should now be Route::Frontend, not the original Route::Deny");
        };
        assert_eq!(rc.cluster_id, None, "promoted Deny stays clusterless");
        assert_eq!(
            rc.redirect,
            RedirectPolicy::Unauthorized,
            "promoted Deny must keep Unauthorized so lookup yields a 401"
        );
        assert!(rc.inherits_listener_hsts);
        let response: Vec<_> = rc.headers_response.iter().collect();
        assert_eq!(response.len(), 1);
        assert_eq!(&*response[0].key, b"strict-transport-security");
        assert_eq!(&*response[0].val, b"max-age=31536000".as_slice());
    }

    #[test]
    fn refresh_inheriting_hsts_skips_lightweight_on_disable() {
        // No HSTS to emit → no allocation of a Route::Frontend just to
        // hold an empty headers_response. The lightweight route is
        // preserved as-is. Three sub-cases cover the disable surface:
        //   - new_hsts == None (operator omitted the field — preserve current… but
        //     this function is called only when the patch DID carry a value, so
        //     None here represents "block was not present"; lightweight stays).
        //   - new_hsts == Some(enabled = Some(false)) (explicit kill switch).
        //   - new_hsts == Some(enabled = Some(true), max_age = None) (malformed
        //     enable — render_hsts returns None, defense-in-depth gate).
        use crate::router::pattern_trie::TrieNode;
        let make_router = || Router {
            pre: vec![(
                DomainRule::Any,
                PathRule::Prefix("/".to_owned()),
                MethodRule::new(None),
                Route::ClusterId("api".to_owned()),
            )],
            tree: TrieNode::root(),
            post: vec![(
                DomainRule::Any,
                PathRule::Prefix("/forbidden".to_owned()),
                MethodRule::new(None),
                Route::Deny,
            )],
        };

        for (label, hsts) in [
            ("none", None),
            (
                "disabled",
                Some(HstsConfig {
                    enabled: Some(false),
                    max_age: None,
                    include_subdomains: None,
                    preload: None,
                    force_replace_backend: None,
                }),
            ),
            (
                "enabled-without-max-age",
                Some(HstsConfig {
                    enabled: Some(true),
                    max_age: None,
                    include_subdomains: None,
                    preload: None,
                    force_replace_backend: None,
                }),
            ),
        ] {
            let mut router = make_router();
            let count = router.refresh_inheriting_hsts(hsts.as_ref());
            assert_eq!(count, 0, "no promotion expected for {label}");
            assert!(
                matches!(router.pre[0].3, Route::ClusterId(_)),
                "{label}: ClusterId must stay lightweight"
            );
            assert!(
                matches!(router.post[0].3, Route::Deny),
                "{label}: Deny must stay lightweight"
            );
        }
    }

    #[test]
    fn refresh_inheriting_hsts_promoted_entry_refreshes_on_subsequent_patches() {
        // First patch promotes ClusterId → Route::Frontend with HSTS.
        // Second patch with a different max-age must refresh the
        // promoted entry through the normal path-1 branch (no double
        // STS edit, no second promotion of an already-promoted entry).
        use crate::router::pattern_trie::TrieNode;
        let mut router = Router {
            pre: Vec::new(),
            tree: TrieNode::root(),
            post: vec![(
                DomainRule::Any,
                PathRule::Prefix("/".to_owned()),
                MethodRule::new(None),
                Route::ClusterId("api".to_owned()),
            )],
        };

        let first_patch = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        };
        assert_eq!(router.refresh_inheriting_hsts(Some(&first_patch)), 1);

        let second_patch = HstsConfig {
            enabled: Some(true),
            max_age: Some(63_072_000),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        assert_eq!(
            router.refresh_inheriting_hsts(Some(&second_patch)),
            1,
            "the previously promoted entry must be re-counted via the path-1 branch"
        );

        let Route::Frontend(rc) = &router.post[0].3 else {
            panic!("post[0] should still be Route::Frontend after the second patch");
        };
        let response: Vec<_> = rc.headers_response.iter().collect();
        assert_eq!(
            response.len(),
            1,
            "second patch must REPLACE the existing STS edit, not append a duplicate"
        );
        assert_eq!(
            &*response[0].val,
            b"max-age=63072000; includeSubDomains".as_slice()
        );
    }

    #[test]
    fn refresh_inheriting_hsts_promoted_entry_loses_hsts_on_disable_patch() {
        // After a promotion, a disable patch must strip the STS edit
        // through the path-1 branch. The Route::Frontend wrapper stays
        // (we don't demote back to Route::ClusterId — the small per-
        // request overhead of running through `from_frontend` instead
        // of the short-circuit is acceptable, and demotion would
        // require carrying additional state).
        use crate::router::pattern_trie::TrieNode;
        let mut router = Router {
            pre: vec![(
                DomainRule::Any,
                PathRule::Prefix("/".to_owned()),
                MethodRule::new(None),
                Route::ClusterId("api".to_owned()),
            )],
            tree: TrieNode::root(),
            post: Vec::new(),
        };

        let enable = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        };
        assert_eq!(router.refresh_inheriting_hsts(Some(&enable)), 1);

        let disable = HstsConfig {
            enabled: Some(false),
            max_age: None,
            include_subdomains: None,
            preload: None,
            force_replace_backend: None,
        };
        assert_eq!(
            router.refresh_inheriting_hsts(Some(&disable)),
            1,
            "the promoted entry must still be touched on disable to strip its STS edit"
        );

        let Route::Frontend(rc) = &router.pre[0].3 else {
            panic!("pre[0] should still be Route::Frontend (no demotion)");
        };
        assert_eq!(rc.cluster_id.as_deref(), Some("api"));
        assert_eq!(
            rc.headers_response.len(),
            0,
            "disable patch must strip the STS edit from the promoted entry"
        );
    }

    #[test]
    fn refresh_inheriting_hsts_promotes_clusterid_in_trie_on_enable() {
        // Trie-leaf coverage for path 2: the existing five tests
        // exercise the `pre`/`post` Vecs only, but the same `visit`
        // closure runs for tree leaves through
        // `tree.for_each_value_mut`. Assert that a `Route::ClusterId`
        // sitting in the trie is promoted in place on an enable
        // patch, with routing semantics preserved.
        use crate::router::pattern_trie::TrieNode;
        let mut router = Router {
            pre: Vec::new(),
            tree: TrieNode::root(),
            post: Vec::new(),
        };
        let path_rule = PathRule::Prefix("/".to_owned());
        let method_rule = MethodRule::new(None);
        assert!(router.add_tree_rule(
            b"example.com",
            &path_rule,
            &method_rule,
            &Route::ClusterId("api".to_owned()),
        ));

        let new_hsts = HstsConfig {
            enabled: Some(true),
            max_age: Some(31_536_000),
            include_subdomains: Some(true),
            preload: None,
            force_replace_backend: None,
        };
        let count = router.refresh_inheriting_hsts(Some(&new_hsts));
        assert_eq!(
            count, 1,
            "trie-resident ClusterId must be promoted + counted"
        );

        let (_, paths) = router
            .tree
            .domain_lookup_mut(b"example.com", false)
            .expect("trie leaf still present after refresh");
        assert_eq!(paths.len(), 1);
        let Route::Frontend(rc) = &paths[0].2 else {
            panic!("trie leaf should now be Route::Frontend, not Route::ClusterId");
        };
        assert_eq!(rc.cluster_id.as_deref(), Some("api"));
        assert_eq!(rc.redirect, RedirectPolicy::Forward);
        assert!(rc.inherits_listener_hsts);
        let response: Vec<_> = rc.headers_response.iter().collect();
        assert_eq!(response.len(), 1);
        assert_eq!(&*response[0].key, b"strict-transport-security");
        assert_eq!(
            &*response[0].val,
            b"max-age=31536000; includeSubDomains".as_slice()
        );
    }

    #[test]
    fn convert_regex() {
        // Compiled regexes are anchored with `\A` … `\z` so `Regex::is_match`
        // (unanchored by default) only succeeds on a full-host match.
        assert_eq!(
            convert_regex_domain_rule("www.example.com")
                .unwrap()
                .as_str(),
            "\\Awww\\.example\\.com\\z"
        );
        assert_eq!(
            convert_regex_domain_rule("*.example.com").unwrap().as_str(),
            "\\A*\\.example\\.com\\z"
        );
        assert_eq!(
            convert_regex_domain_rule("test.*.example.com")
                .unwrap()
                .as_str(),
            "\\Atest\\.*\\.example\\.com\\z"
        );
        assert_eq!(
            convert_regex_domain_rule("css./cdn[a-z0-9]+/.example.com")
                .unwrap()
                .as_str(),
            "\\Acss\\.cdn[a-z0-9]+\\.example\\.com\\z"
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

    /// Compiled regex rules must reject suffix / prefix matches. Without
    /// `\A` / `\z` anchors, `Regex::is_match` treats the pattern as
    /// "match anywhere in the haystack", letting `attacker.example.com.evil.org`
    /// reach a frontend that only intends to serve `example.com`.
    #[test]
    fn regex_domain_rule_rejects_suffix_and_prefix() {
        let rule: DomainRule = "/example\\.com/".parse().unwrap();
        assert!(rule.matches(b"example.com"));
        assert!(!rule.matches(b"attacker.example.com"));
        assert!(!rule.matches(b"example.com.evil.org"));
        assert!(!rule.matches(b"prefixexample.com"));
        assert!(!rule.matches(b"example.commercial"));
    }

    /// A multi-segment regex hostname (alternating regex and literal
    /// subdomains) must keep each segment confined: only the first `/`
    /// after the opening delimiter closes a regex segment. A missing
    /// `break` collapsed every later `/` into the same segment, swallowing
    /// the literal `.` separators between them.
    #[test]
    fn regex_domain_rule_multi_segment_segments_are_isolated() {
        let pattern = convert_regex_domain_rule("/seg1/.foo./seg2/.com")
            .expect("multi-segment regex hostname must compile");
        assert_eq!(pattern.as_str(), "\\Aseg1\\.foo\\.seg2\\.com\\z");
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
            DomainRule::Regex(Regex::new("\\Acdn[0-9]+\\.example\\.com\\z").unwrap())
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
        assert!(
            !DomainRule::Wildcard("*.example.com".to_string())
                .matches("test.www.example.com".as_bytes())
        );
        assert!(
            "/cdn[0-9]+/.example.com"
                .parse::<DomainRule>()
                .unwrap()
                .matches("cdn1.example.com".as_bytes())
        );
        assert!(
            !"/cdn[0-9]+/.example.com"
                .parse::<DomainRule>()
                .unwrap()
                .matches("www.example.com".as_bytes())
        );
        assert!(
            !"/cdn[0-9]+/.example.com"
                .parse::<DomainRule>()
                .unwrap()
                .matches("cdn10.exampleAcom".as_bytes())
        );
    }

    #[test]
    fn match_domain_rule_wildcard_short_hostname_does_not_panic() {
        let rule = DomainRule::Wildcard("*.foo.example.com".to_string());

        // Regression for issue #1223: an empty hostname must not panic and must not match.
        assert!(!rule.matches(b""));

        // Hostname strictly shorter than the suffix must not panic and must not match.
        assert!(!rule.matches(b"a.b"));
        assert!(!rule.matches(b"x"));

        // Boundary: hostname equal to the suffix (s without the leading '*') has an empty
        // leftmost label. RFC 1035 §3.1 forbids empty labels, so reject.
        assert!(!rule.matches(b".foo.example.com"));

        // Multi-label leftmost prefix (existing intent — single-label only).
        assert!(!rule.matches(b"y.x.foo.example.com"));

        // Single-label leftmost prefix — must still match (this is the happy case the
        // pre-existing match_domain_rule already covers, repeated here for symmetry).
        assert!(rule.matches(b"x.foo.example.com"));
    }

    #[test]
    fn router_lookup_wildcard_pre_rule_short_hostname_does_not_panic() {
        let mut router = Router::new();

        // Wildcard in a pre-rule routes through DomainRule::Wildcard::matches
        // (not the trie). This is the path that panicked on issue #1223.
        assert!(router.add_pre_rule(
            &"*.foo.example.com".parse::<DomainRule>().unwrap(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("wildcard".to_string()),
        ));

        let method = Method::new(&b"GET"[..]);

        // Issue #1223: short hostnames must not panic Router::lookup.
        assert!(router.lookup("", "/", &method).is_err());
        assert!(router.lookup("x", "/", &method).is_err());
        assert!(router.lookup("a.b", "/", &method).is_err());

        // Boundary: hostname equal to the suffix has an empty leftmost label.
        assert!(router.lookup(".foo.example.com", "/", &method).is_err());

        // Happy case: single-label leftmost matches via the pre-rule path.
        assert_eq!(
            router.lookup("x.foo.example.com", "/", &method),
            Ok(RouteResult::forward("wildcard".to_string()))
        );
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
            &Route::ClusterId("base".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router.lookup("www.sozu.io", "/api", &Method::Get),
            Ok(RouteResult::forward("base".to_string()))
        );
        assert!(router.add_tree_rule(
            b"*.sozu.io",
            &PathRule::Prefix("/api".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("api".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router.lookup("www.sozu.io", "/ap", &Method::Get),
            Ok(RouteResult::forward("base".to_string()))
        );
        assert_eq!(
            router.lookup("www.sozu.io", "/api", &Method::Get),
            Ok(RouteResult::forward("api".to_string()))
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
            &Route::ClusterId("base".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router.lookup("www.sozu.io", "/api", &Method::Get),
            Ok(RouteResult::forward("base".to_string()))
        );
        assert!(router.add_tree_rule(
            b"api.sozu.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("api".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router.lookup("www.sozu.io", "/api", &Method::Get),
            Ok(RouteResult::forward("base".to_string()))
        );
        assert_eq!(
            router.lookup("api.sozu.io", "/api", &Method::Get),
            Ok(RouteResult::forward("api".to_string()))
        );
    }

    /// A malformed hostname arriving from the control plane
    /// (`AddHttpFrontend` over the command socket, or a `LoadState`
    /// replay) must be REJECTED, never panic the worker. `TrieNode::insert`
    /// used to `assert_ne!` on `InsertResult::Failed`, so a frontend whose
    /// hostname ended in `/` killed every worker the master fanned the
    /// request out to -- and killed them again on each restart replay.
    #[test]
    fn add_tree_rule_rejects_malformed_hostnames_without_panicking() {
        // Every shape here reached `InsertResult::Failed` (or an empty
        // `partial_key`) inside `insert_recursive`.
        for hostname in [
            &b"example.com/"[..],
            b"www.example.com/",
            b"foo/",
            b"a/*/",
            b"/",
            b"///",
            b"abc/[0-9]+/.example.com",
            b"/[/.example.com",
            // Named as rejected in `doc/configure.md`'s regex-hostname
            // section: a leading `.` before a regex segment is an empty label.
            b"./test[0-9]/.example.com",
            b".example.com",
            b".a.b",
            b"..",
        ] {
            let mut router = Router::new();
            assert!(
                !router.add_tree_rule(
                    hostname,
                    &PathRule::Prefix("/".to_string()),
                    &MethodRule::new(Some("GET".to_string())),
                    &Route::ClusterId("cluster".to_string()),
                ),
                "{:?} must be rejected, not inserted",
                String::from_utf8_lossy(hostname),
            );
            // A rejected add must leave NO trace: the route table is
            // exactly as empty as it was before the attempt.
            assert!(
                router.tree.is_empty(),
                "{:?} was rejected but still mutated the route table",
                String::from_utf8_lossy(hostname),
            );
        }
    }

    /// The same rejection, one layer up: `add_http_front` must surface a
    /// `RouterError::AddRoute` so the worker answers the master with a
    /// failure instead of dying.
    #[test]
    fn add_http_front_surfaces_a_malformed_hostname_as_an_error() {
        let mut router = Router::new();
        let front = HttpFrontend {
            hostname: "example.com/".to_owned(),
            ..test_http_frontend()
        };
        assert!(matches!(
            router.add_http_front(&front),
            Err(RouterError::AddRoute(_))
        ));
        assert!(router.tree.is_empty());

        // A hostname whose last segment is followed by a bare trailing `.`
        // is rejected one layer earlier still, by the unconditional
        // `DomainRule` parse -- which used to walk out of bounds on it
        // (see `domain_rule_rejects_a_trailing_dot_after_a_regex_segment`).
        for hostname in ["/a/.", "a/b/.", "x./y/."] {
            let mut router = Router::new();
            let front = HttpFrontend {
                hostname: (*hostname).to_owned(),
                ..test_http_frontend()
            };
            assert!(
                matches!(
                    router.add_http_front(&front),
                    Err(RouterError::InvalidDomain { .. })
                ),
                "{hostname:?} must be rejected as an invalid domain, not panic",
            );
            assert!(router.tree.is_empty());
        }
    }

    /// `convert_regex_domain_rule` used to walk one byte past the end of a
    /// hostname whose last segment is followed by a bare trailing `.`
    /// (`/a/.`, `a/b/.`, `x./y/.`): the `.` arm of the loop tail advances
    /// `index` to `s.len()` and the next iteration indexed `s[index]` out
    /// of bounds -- a release panic (slice bounds checks never compile
    /// out) reachable from the control plane through the unconditional
    /// `DomainRule` parse in `add_http_front`, sidestepping every trie
    /// guard.
    #[test]
    fn domain_rule_rejects_a_trailing_dot_after_a_regex_segment() {
        for hostname in ["/a/.", "a/b/.", "/[/.", "x./y/."] {
            assert!(
                hostname.parse::<DomainRule>().is_err(),
                "{hostname:?} must be rejected, not panic",
            );
        }
        // The guard must not disturb the legitimate `.`-anchored
        // regex-segment grammar.
        assert!("abc./[0-9]+/.example.com".parse::<DomainRule>().is_ok());
    }

    /// The trie recurses once per label and the regex-segment grammar
    /// compiles control-plane-supplied patterns, so both add and remove
    /// bound the hostname to [`MAX_HOSTNAME_LENGTH`] before parsing
    /// anything: a ~100k-label hostname otherwise aborts the worker with
    /// an uncatchable stack overflow, and regex compilation time grows
    /// with the pattern (a 2 MiB hostname stalls the single-threaded
    /// worker for hundreds of milliseconds before rejection).
    #[test]
    fn add_http_front_rejects_an_oversized_hostname() {
        // At the bound: accepted.
        let mut router = Router::new();
        let front = HttpFrontend {
            hostname: "a".repeat(MAX_HOSTNAME_LENGTH),
            ..test_http_frontend()
        };
        assert!(router.add_http_front(&front).is_ok());

        // One byte over: rejected on add AND on remove, before any parse.
        let mut router = Router::new();
        let front = HttpFrontend {
            hostname: "a".repeat(MAX_HOSTNAME_LENGTH + 1),
            ..test_http_frontend()
        };
        assert!(matches!(
            router.add_http_front(&front),
            Err(RouterError::InvalidDomain { .. })
        ));
        assert!(matches!(
            router.remove_http_front(&front),
            Err(RouterError::InvalidDomain { .. })
        ));
        assert!(router.tree.is_empty());

        // A label-count bomb is caught by the same byte bound.
        let front = HttpFrontend {
            hostname: "a.".repeat(MAX_HOSTNAME_LENGTH),
            ..test_http_frontend()
        };
        assert!(matches!(
            router.add_http_front(&front),
            Err(RouterError::InvalidDomain { .. })
        ));
    }

    #[test]
    fn router_insert_remove_through_regex() {
        let mut router = Router::new();

        assert!(router.add_tree_rule(
            b"www./.*/.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("base".to_string())
        ));
        println!("{:#?}", router.tree);
        assert!(router.add_tree_rule(
            b"www.doc./.*/.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("doc".to_string())
        ));
        println!("{:#?}", router.tree);
        assert_eq!(
            router.lookup("www.sozu.io", "/", &Method::Get),
            Ok(RouteResult::forward("base".to_string()))
        );
        assert_eq!(
            router.lookup("www.doc.sozu.io", "/", &Method::Get),
            Ok(RouteResult::forward("doc".to_string()))
        );
        assert!(router.remove_tree_rule(
            b"www./.*/.io",
            &PathRule::Prefix("".to_string()),
            &MethodRule::new(Some("GET".to_string()))
        ));
        println!("{:#?}", router.tree);
        assert!(router.lookup("www.sozu.io", "/", &Method::Get).is_err());
        assert_eq!(
            router.lookup("www.doc.sozu.io", "/", &Method::Get),
            Ok(RouteResult::forward("doc".to_string()))
        );
    }

    #[test]
    fn match_router() {
        let mut router = Router::new();

        assert!(router.add_pre_rule(
            &"*".parse::<DomainRule>().unwrap(),
            &PathRule::Prefix("/.well-known/acme-challenge".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("acme".to_string())
        ));
        assert!(router.add_tree_rule(
            "www.example.com".as_bytes(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("example".to_string())
        ));
        assert!(router.add_tree_rule(
            "*.test.example.com".as_bytes(),
            &PathRule::Regex(Regex::new("/hello[A-Z]+/").unwrap()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("examplewildcard".to_string())
        ));
        assert!(router.add_tree_rule(
            "/test[0-9]/.example.com".as_bytes(),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("exampleregex".to_string())
        ));

        assert_eq!(
            router.lookup("www.example.com", "/helloA", &Method::new(&b"GET"[..])),
            Ok(RouteResult::forward("example".to_string()))
        );
        assert_eq!(
            router.lookup(
                "www.example.com",
                "/.well-known/acme-challenge",
                &Method::new(&b"GET"[..])
            ),
            Ok(RouteResult::forward("acme".to_string()))
        );
        assert!(
            router
                .lookup("www.test.example.com", "/", &Method::new(&b"GET"[..]))
                .is_err()
        );
        assert_eq!(
            router.lookup(
                "www.test.example.com",
                "/helloAB/",
                &Method::new(&b"GET"[..])
            ),
            Ok(RouteResult::forward("examplewildcard".to_string()))
        );
        assert_eq!(
            router.lookup("test1.example.com", "/helloAB/", &Method::new(&b"GET"[..])),
            Ok(RouteResult::forward("exampleregex".to_string()))
        );
    }

    #[test]
    fn has_hostname_checks_tree_pre_and_post() {
        let mut router = Router::new();

        // Empty router has no hostnames
        assert!(!router.has_hostname("www.example.com"));

        // Add a tree rule
        assert!(router.add_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("cluster1".to_string())
        ));
        assert!(router.has_hostname("www.example.com"));
        assert!(!router.has_hostname("api.example.com"));

        // Remove the tree rule — hostname should disappear
        assert!(router.remove_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string()))
        ));
        assert!(!router.has_hostname("www.example.com"));

        // Add a pre rule with an exact domain
        assert!(router.add_pre_rule(
            &DomainRule::Exact("api.example.com".to_string()),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(None),
            &Route::ClusterId("cluster2".to_string())
        ));
        assert!(router.has_hostname("api.example.com"));
        assert!(!router.has_hostname("www.example.com"));

        // Add a post rule
        assert!(router.add_post_rule(
            &DomainRule::Exact("cdn.example.com".to_string()),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(None),
            &Route::ClusterId("cluster3".to_string())
        ));
        assert!(router.has_hostname("cdn.example.com"));

        // Remove pre rule, post rule should still be detected
        assert!(router.remove_pre_rule(
            &DomainRule::Exact("api.example.com".to_string()),
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(None),
        ));
        assert!(!router.has_hostname("api.example.com"));
        assert!(router.has_hostname("cdn.example.com"));
    }

    #[test]
    fn has_hostname_false_after_last_route_removed() {
        let mut router = Router::new();

        // Add two routes for the same hostname with different paths
        assert!(router.add_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("cluster1".to_string())
        ));
        assert!(router.add_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/api".to_string()),
            &MethodRule::new(Some("GET".to_string())),
            &Route::ClusterId("cluster2".to_string())
        ));
        assert!(router.has_hostname("www.example.com"));

        // Remove first route — hostname should still exist
        assert!(router.remove_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/".to_string()),
            &MethodRule::new(Some("GET".to_string()))
        ));
        assert!(router.has_hostname("www.example.com"));

        // Remove second route — hostname should be gone
        assert!(router.remove_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/api".to_string()),
            &MethodRule::new(Some("GET".to_string()))
        ));
        assert!(!router.has_hostname("www.example.com"));
    }
    /// The operator-visible half of the trie's leftmost-regex insert gap.
    /// A frontend whose leftmost host segment is a regex
    /// (`/test[0-9]/.example.com`) could not be added once a DEEPER
    /// frontend sharing that segment (`foo./test[0-9]/.example.com`) had
    /// already opened it: `TrieNode::insert_recursive` answered `Existing`
    /// for a host it stored nowhere.
    ///
    /// The two profiles failed differently, which is why this test asserts
    /// the STORE rather than the return code alone. In debug the
    /// post-insert reachability `debug_assert!` below `domain_insert`
    /// killed the worker outright — "a freshly inserted tree domain must
    /// resolve to its inserted rule" — on a control-plane `AddHttpFrontend`
    /// (and again on every `LoadState` replay). In release that assert is
    /// compiled out, so `add_tree_rule` returned `true`, the CLI reported
    /// OK and `sozu query frontends` listed the route, while the trie never
    /// served it.
    ///
    /// To SEE THIS RED: in the `pos == 0` arm of the dedup loop in
    /// `TrieNode::insert_recursive` (`router/pattern_trie.rs`), replace
    /// `return t.1.insert_own_value(key, value);` with
    /// `return InsertResult::Existing;`. In release the `lookup` assertion
    /// below fails with `Err(route_not_found …)`; in debug the
    /// `debug_assert!` in `add_tree_rule` panics first at
    /// "a freshly inserted tree domain must resolve to its inserted rule"
    /// — comment that `debug_assert!` out and debug lands on the same
    /// `lookup` assertion release does.
    #[test]
    fn a_leftmost_regex_frontend_is_addable_after_a_deeper_sibling() {
        let path = PathRule::Prefix("/".to_string());
        let method = MethodRule::new(Some("GET".to_string()));

        // Order A: deeper frontend first — the order that used to lose the
        // leftmost host.
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"foo./test[0-9]/.example.com",
            &path,
            &method,
            &Route::ClusterId("deeper".to_string()),
        ));
        assert!(
            router.add_tree_rule(
                b"/test[0-9]/.example.com",
                &path,
                &method,
                &Route::ClusterId("leftmost".to_string()),
            ),
            "a leftmost-regex frontend must be accepted after a deeper sibling",
        );
        assert_eq!(
            router.lookup("test4.example.com", "/", &Method::Get),
            Ok(RouteResult::forward("leftmost".to_string())),
            "the leftmost-regex frontend must actually route, not just report OK",
        );
        assert_eq!(
            router.lookup("foo.test4.example.com", "/", &Method::Get),
            Ok(RouteResult::forward("deeper".to_string())),
            "the deeper frontend must still route",
        );

        // Order B: leftmost frontend first — the order that already worked.
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"/test[0-9]/.example.com",
            &path,
            &method,
            &Route::ClusterId("leftmost".to_string()),
        ));
        assert!(router.add_tree_rule(
            b"foo./test[0-9]/.example.com",
            &path,
            &method,
            &Route::ClusterId("deeper".to_string()),
        ));
        assert_eq!(
            router.lookup("test4.example.com", "/", &Method::Get),
            Ok(RouteResult::forward("leftmost".to_string())),
        );
        assert_eq!(
            router.lookup("foo.test4.example.com", "/", &Method::Get),
            Ok(RouteResult::forward("deeper".to_string())),
        );

        // And removal through the public surface still takes exactly one.
        assert!(router.remove_tree_rule(b"/test[0-9]/.example.com", &path, &method));
        assert!(
            router
                .lookup("test4.example.com", "/", &Method::Get)
                .is_err()
        );
        assert_eq!(
            router.lookup("foo.test4.example.com", "/", &Method::Get),
            Ok(RouteResult::forward("deeper".to_string())),
        );
    }

    /// `add_tree_rule` returning `true` is a claim that the route table now
    /// serves the rule. The trie's leftmost-regex gap broke exactly that
    /// contract in RELEASE builds, where the post-insert reachability
    /// `debug_assert!` is compiled out: the control plane reported success
    /// and the data plane never routed the host.
    ///
    /// This pins the contract itself, so it fails in a release build too —
    /// no `debug_assert` is involved in the assertion. Every shape here is
    /// one the trie stores through a different arm (literal, wildcard,
    /// leftmost regex, deeper regex, regex opened by a deeper sibling), and
    /// each is checked BOTH ways round: a reported success must route, and
    /// a reported failure must leave nothing behind.
    ///
    /// To SEE THIS RED: in the `pos == 0` arm of the dedup loop in
    /// `TrieNode::insert_recursive`, replace
    /// `return t.1.insert_own_value(key, value);` with
    /// `return InsertResult::Existing;`. The `/test[0-9]/.example.com` row
    /// added after `foo./test[0-9]/.example.com` then reports `true` while
    /// `lookup` answers `Err(route_not_found …)`, failing the
    /// "reported success but does not route" assertion in release. In debug
    /// the `debug_assert!` in `add_tree_rule` fires first with
    /// "a freshly inserted tree domain must resolve to its inserted rule".
    #[test]
    fn add_tree_rule_never_reports_success_for_a_rule_it_did_not_store() {
        let path = PathRule::Prefix("/".to_string());
        let method = MethodRule::new(Some("GET".to_string()));

        // (hostname to add, hostname that must then route)
        let sequence: &[(&[u8], &str)] = &[
            (b"www.example.com", "www.example.com"),
            (b"*.wild.example.com", "any.wild.example.com"),
            (b"foo./test[0-9]/.example.com", "foo.test4.example.com"),
            // Opened above by its deeper sibling: the regression.
            (b"/test[0-9]/.example.com", "test4.example.com"),
            (b"bar./test[0-9]/.example.com", "bar.test7.example.com"),
        ];

        let mut router = Router::new();
        for (index, (hostname, routable)) in sequence.iter().enumerate() {
            let cluster = format!("cluster{index}");
            let added =
                router.add_tree_rule(hostname, &path, &method, &Route::ClusterId(cluster.clone()));
            let resolved = router.lookup(routable, "/", &Method::Get);
            assert!(
                added,
                "{:?} must be accepted",
                String::from_utf8_lossy(hostname),
            );
            assert_eq!(
                resolved,
                Ok(RouteResult::forward(cluster)),
                "add_tree_rule reported success for {:?} but it does not route",
                String::from_utf8_lossy(hostname),
            );
        }

        // The converse half of the contract: a REJECTED add must not be
        // reported as stored either, and must leave no route behind.
        for hostname in [
            &b"example.com/"[..],
            b".example.com",
            b"abc/[0-9]+/.example.com",
        ] {
            let mut router = Router::new();
            assert!(
                !router.add_tree_rule(
                    hostname,
                    &path,
                    &method,
                    &Route::ClusterId("rejected".to_string()),
                ),
                "{:?} must be rejected",
                String::from_utf8_lossy(hostname),
            );
            assert!(
                router.tree.is_empty(),
                "{:?} was rejected but still mutated the route table",
                String::from_utf8_lossy(hostname),
            );
        }
    }

    // ---- Rule-ordering contract (`doc/configure.md`, "When declaration order
    // decides"). The path rules of one hostname are a flat `Vec` at the trie
    // leaf, scanned once in declaration order: `Regex`/`Equals` RETURN on the
    // first match, while `Prefix` accumulates the longest. Nothing pinned any
    // of that, and the documented rule has been wrong three times, so each
    // ordering below is its own case.

    /// Declare one tree rule for `www.example.com`. `method` is a PARAMETER,
    /// never a constant: whether a rule carries one decides if it ends the
    /// lookup scan early, so holding it fixed hides half the orderings below.
    fn add_path_rule(
        router: &mut Router,
        path: PathRule,
        method: Option<&str>,
        cluster: &str,
    ) -> bool {
        router.add_tree_rule(
            b"www.example.com",
            &path,
            &MethodRule::new(method.map(str::to_owned)),
            &Route::ClusterId(cluster.to_owned()),
        )
    }

    /// The cluster a `GET` for `path` on `www.example.com` resolves to.
    fn routed_cluster(router: &Router, path: &str) -> Option<String> {
        router
            .lookup("www.example.com", path, &Method::Get)
            .ok()
            .and_then(|result| result.cluster_id)
    }

    /// Declare `first` then `second` — each with its own `method` — and resolve
    /// `path`. Declaration order is the variable under test, so callers assert
    /// both orders.
    fn winner_of(
        first: (PathRule, Option<&str>, &str),
        second: (PathRule, Option<&str>, &str),
        path: &str,
    ) -> Option<String> {
        let mut router = Router::new();
        assert!(add_path_rule(&mut router, first.0, first.1, first.2));
        assert!(add_path_rule(&mut router, second.0, second.1, second.2));
        routed_cluster(&router, path)
    }

    /// `Some("GET")` — a rule whose method matches the request, so it ends the
    /// scan. Spelled out at every call site so the axis stays visible.
    const GET: Option<&str> = Some("GET");
    /// `None` — a rule with no method. It matches every request but does NOT
    /// end the scan.
    const NO_METHOD: Option<&str> = None;
    /// `Some("POST")` against a `GET` request — a method that is PRESENT and
    /// does NOT match, i.e. `MethodRuleResult::None`. The third value of the
    /// method axis: such a rule is skipped entirely, whatever its `path_type`.
    /// Covering only `GET` and `NO_METHOD` left every claim about this value
    /// untested, which is how two false sentences reached the documentation.
    const WRONG_METHOD: Option<&str> = Some("POST");

    /// Among `PREFIX` path rules the LONGEST match wins, and that is the one
    /// genuinely order-independent case in the router.
    ///
    /// To SEE THIS RED: in `Router::lookup`'s path-rule loop, replace the
    /// `PathRuleResult::Prefix(size) => { if size >= prefix_length {` guard
    /// with `if matched.is_none() {` so the FIRST matching prefix wins instead
    /// of the longest. The `SHORT`-first case below then resolves to `SHORT`.
    #[test]
    fn the_longest_prefix_wins_whichever_prefix_is_declared_first() {
        let short = || PathRule::Prefix("/a".to_owned());
        let long = || PathRule::Prefix("/ab".to_owned());

        assert_eq!(
            winner_of((short(), GET, "SHORT"), (long(), GET, "LONG"), "/abc").as_deref(),
            Some("LONG"),
            "the longer prefix must win even when the shorter one was declared first",
        );
        assert_eq!(
            winner_of((long(), GET, "LONG"), (short(), GET, "SHORT"), "/abc").as_deref(),
            Some("LONG"),
            "the longer prefix must win when it was declared first too",
        );
    }

    /// Two `PREFIX` rules can only tie on length by carrying the SAME prefix
    /// string and differing on `method` — `add_tree_rule` dedups on
    /// `(path, method)`, so that pair coexists. The scan keeps the LAST such
    /// rule, because the guard is `size >= prefix_length`, not `>`.
    ///
    /// This is the subtlest ordering in the router and the easiest to "fix"
    /// by accident: tightening `>=` to `>` looks like a harmless no-op that
    /// stops a rule overwriting an equally-good one, and silently flips which
    /// cluster serves the request.
    ///
    /// To SEE THIS RED: change `if size >= prefix_length {` to
    /// `if size > prefix_length {` in `Router::lookup`'s path-rule loop. Both
    /// assertions below then resolve to the FIRST declared rule.
    #[test]
    fn equal_length_prefixes_differing_only_by_method_resolve_to_the_last_declared() {
        let declare = |first_is_get: bool| {
            let mut router = Router::new();
            let get = (
                MethodRule::new(Some("GET".to_owned())),
                Route::ClusterId("GET-RULE".to_owned()),
            );
            let all = (
                MethodRule::new(None),
                Route::ClusterId("ALL-RULE".to_owned()),
            );
            let (a, b) = if first_is_get { (get, all) } else { (all, get) };
            for (method, route) in [a, b] {
                assert!(router.add_tree_rule(
                    b"www.example.com",
                    &PathRule::Prefix("/ab".to_owned()),
                    &method,
                    &route,
                ));
            }
            routed_cluster(&router, "/abc")
        };

        assert_eq!(
            declare(true).as_deref(),
            Some("ALL-RULE"),
            "the LAST equal-length prefix declared must win (`>=`, not `>`)",
        );
        assert_eq!(
            declare(false).as_deref(),
            Some("GET-RULE"),
            "the LAST equal-length prefix declared must win in the other order too",
        );
    }

    /// Two overlapping `REGEX` path rules are decided by declaration order:
    /// the scan returns on the first match, and nothing weighs one pattern as
    /// more specific than another. A wider pattern declared first therefore
    /// shadows a narrower one declared later.
    ///
    /// To SEE THIS RED: in `Router::lookup`'s path-rule loop, replace the
    /// `MethodRuleResult::Equals => { return Ok(RouteResult::new_with_trie(..)) }`
    /// arm under `PathRuleResult::Regex | PathRuleResult::Equals` with the
    /// non-returning body its `MethodRuleResult::All` sibling uses
    /// (`prefix_length = path_b.len(); matched = Some((rule, route));`). Losing
    /// the early return makes the LAST match win, so both assertions flip.
    #[test]
    fn with_a_matching_method_the_first_declared_regex_path_rule_wins() {
        let wide = || PathRule::Regex(Regex::new("/a.*").expect("test regex must compile"));
        let narrow = || PathRule::Regex(Regex::new("/ab.*").expect("test regex must compile"));

        assert_eq!(
            winner_of((wide(), GET, "WIDE"), (narrow(), GET, "NARROW"), "/abc").as_deref(),
            Some("WIDE"),
            "the first-declared regex must win even though the later one is narrower",
        );
        assert_eq!(
            winner_of((narrow(), GET, "NARROW"), (wide(), GET, "WIDE"), "/abc").as_deref(),
            Some("NARROW"),
            "reversing the declaration order reverses the winner",
        );
    }

    /// `EQUALS` holds no priority over `REGEX`: they share one match arm and
    /// the scan returns on whichever comes first. An exact-match rule does NOT
    /// outrank a pattern that was declared before it.
    ///
    /// To SEE THIS RED: the same mutation as
    /// `with_a_matching_method_the_first_declared_regex_path_rule_wins`
    /// — drop the early `return` from the `MethodRuleResult::Equals` arm under
    /// `PathRuleResult::Regex | PathRuleResult::Equals`. Both assertions flip
    /// to the last declared rule.
    #[test]
    fn with_a_matching_method_neither_equals_nor_regex_outranks_the_other() {
        let equals = || PathRule::Equals("/abc".to_owned());
        let regex = || PathRule::Regex(Regex::new("/a.*").expect("test regex must compile"));

        assert_eq!(
            winner_of((equals(), GET, "EQUALS"), (regex(), GET, "REGEX"), "/abc").as_deref(),
            Some("EQUALS"),
            "EQUALS declared first must win",
        );
        assert_eq!(
            winner_of((regex(), GET, "REGEX"), (equals(), GET, "EQUALS"), "/abc").as_deref(),
            Some("REGEX"),
            "REGEX declared first must win — EQUALS has no inherent priority",
        );
    }

    /// A matching `EQUALS` or `REGEX` beats a `PREFIX` rule in either order,
    /// even when the prefix is the longer, more specific pattern: the early
    /// return short-circuits the scan before any prefix accumulation is read.
    /// This is the one cross-type precedence the router really does have.
    ///
    /// To SEE THIS RED: the same mutation again — drop the early `return` from
    /// the `MethodRuleResult::Equals` arm under
    /// `PathRuleResult::Regex | PathRuleResult::Equals`. That arm's
    /// replacement sets `prefix_length = path_b.len()`, so the equally-long
    /// `PREFIX` rule then satisfies `size >= prefix_length` and overwrites it;
    /// the `PREFIX`-declared-second cases below resolve to `PREFIX`.
    #[test]
    fn with_a_matching_method_equals_or_regex_beats_a_longer_prefix_either_order() {
        let prefix = || PathRule::Prefix("/abc".to_owned());
        let regex = || PathRule::Regex(Regex::new("/a.*").expect("test regex must compile"));
        let equals = || PathRule::Equals("/abc".to_owned());

        assert_eq!(
            winner_of((prefix(), GET, "PREFIX"), (regex(), GET, "REGEX"), "/abc").as_deref(),
            Some("REGEX"),
            "a regex must beat a longer prefix declared before it",
        );
        assert_eq!(
            winner_of((regex(), GET, "REGEX"), (prefix(), GET, "PREFIX"), "/abc").as_deref(),
            Some("REGEX"),
            "a regex must beat a longer prefix declared after it",
        );
        assert_eq!(
            winner_of((prefix(), GET, "PREFIX"), (equals(), GET, "EQUALS"), "/abc").as_deref(),
            Some("EQUALS"),
            "an exact match must beat an equally long prefix declared before it",
        );
        assert_eq!(
            winner_of((equals(), GET, "EQUALS"), (prefix(), GET, "PREFIX"), "/abc").as_deref(),
            Some("EQUALS"),
            "an exact match must beat an equally long prefix declared after it",
        );
    }

    /// Without a `method`, an `EQUALS` or `REGEX` rule does NOT end the scan:
    /// its match is only recorded, and a later match overwrites it. So the
    /// ordering is the exact REVERSE of the matching-method case — the LAST
    /// declared wins.
    ///
    /// This is the dimension the first six ordering tests all missed, because
    /// their helper hard-coded `Some("GET")`. `HttpFrontend.method` is an
    /// `Option<String>` and both production call sites build the rule with
    /// `MethodRule::new(front.method.clone())`, so a frontend declared without
    /// a method lands here — it is the default shape, not an exotic one.
    ///
    /// To SEE THIS RED: in `Router::lookup`'s path-rule loop, give the
    /// `MethodRuleResult::All` arm under `PathRuleResult::Regex |
    /// PathRuleResult::Equals` the early `return` its `MethodRuleResult::Equals`
    /// sibling has. Method-less rules then short-circuit too and both
    /// assertions flip to the FIRST declared.
    #[test]
    fn without_a_method_the_last_declared_regex_or_equals_wins_not_the_first() {
        let wide = || PathRule::Regex(Regex::new("/a.*").expect("test regex must compile"));
        let narrow = || PathRule::Regex(Regex::new("/ab.*").expect("test regex must compile"));
        let equals = || PathRule::Equals("/abc".to_owned());

        assert_eq!(
            winner_of(
                (wide(), NO_METHOD, "WIDE"),
                (narrow(), NO_METHOD, "NARROW"),
                "/abc"
            )
            .as_deref(),
            Some("NARROW"),
            "without a method the LAST declared regex must win",
        );
        assert_eq!(
            winner_of(
                (narrow(), NO_METHOD, "NARROW"),
                (wide(), NO_METHOD, "WIDE"),
                "/abc"
            )
            .as_deref(),
            Some("WIDE"),
            "reversing the order reverses the winner — still the last declared",
        );
        assert_eq!(
            winner_of(
                (equals(), NO_METHOD, "EQUALS"),
                (wide(), NO_METHOD, "REGEX"),
                "/abc"
            )
            .as_deref(),
            Some("REGEX"),
            "without a method an EQUALS declared first loses to a later REGEX",
        );
        assert_eq!(
            winner_of(
                (wide(), NO_METHOD, "REGEX"),
                (equals(), NO_METHOD, "EQUALS"),
                "/abc"
            )
            .as_deref(),
            Some("EQUALS"),
            "and the reverse order reverses that too",
        );
    }

    /// Without a `method`, a matching `EQUALS`/`REGEX` does not reliably outrank
    /// a `PREFIX` either — the documented cross-type precedence INVERTS. Because
    /// the non-returning arm records `prefix_length = path_b.len()`, a `PREFIX`
    /// declared afterwards overwrites it exactly when its prefix covers the
    /// WHOLE request path; a shorter prefix cannot clear that bar and leaves the
    /// regex standing.
    ///
    /// "Whole request path" means the request-target as it arrives, QUERY
    /// STRING INCLUDED — kawa's `parse_origin_form` hands the router
    /// `/index.html?k=v#h` verbatim. So the identical configuration routes the
    /// opposite way once the client appends `?x=1`: `/abc` no longer spans
    /// `/abc?x=1`, the prefix stops clearing the bar, and the regex wins. The
    /// last case below pins that, because the worked example in
    /// `doc/configure.md` would otherwise teach `/abc` as "the whole path".
    ///
    /// To SEE THIS RED: in `Router::lookup`'s path-rule loop, change the
    /// `MethodRuleResult::All` arm under `PathRuleResult::Regex |
    /// PathRuleResult::Equals` from `prefix_length = path_b.len();` to
    /// `prefix_length = 0;`. The first two assertions still pass; the third
    /// fails first, with `left: Some("SHORT-PREFIX"), right: Some("REGEX")`,
    /// because the shorter `/a` prefix now clears the bar. That one mutation
    /// also breaks the query-string case below it — delete the third assertion
    /// and the fourth fails with `left: Some("PREFIX"), right: Some("REGEX")`,
    /// the prefix having overwritten the regex it should no longer span.
    #[test]
    fn without_a_method_only_a_whole_path_prefix_declared_later_beats_a_regex() {
        let regex = || PathRule::Regex(Regex::new("/a.*").expect("test regex must compile"));
        let whole_path = || PathRule::Prefix("/abc".to_owned());
        let shorter = || PathRule::Prefix("/a".to_owned());

        assert_eq!(
            winner_of(
                (regex(), NO_METHOD, "REGEX"),
                (whole_path(), NO_METHOD, "PREFIX"),
                "/abc"
            )
            .as_deref(),
            Some("PREFIX"),
            "a whole-path prefix declared after a method-less regex must overwrite it",
        );
        assert_eq!(
            winner_of(
                (whole_path(), NO_METHOD, "PREFIX"),
                (regex(), NO_METHOD, "REGEX"),
                "/abc"
            )
            .as_deref(),
            Some("REGEX"),
            "declared before it, the same prefix loses — the last writer wins",
        );
        assert_eq!(
            winner_of(
                (regex(), NO_METHOD, "REGEX"),
                (shorter(), NO_METHOD, "SHORT-PREFIX"),
                "/abc"
            )
            .as_deref(),
            Some("REGEX"),
            "a prefix shorter than the request path must NOT displace the regex",
        );

        // Same rules, same declaration order — only the request gains a query
        // string, and the winner inverts. The request path the router matches
        // is `/abc?x=1`, which `/abc` no longer covers.
        assert_eq!(
            winner_of(
                (regex(), NO_METHOD, "REGEX"),
                (whole_path(), NO_METHOD, "PREFIX"),
                "/abc?x=1"
            )
            .as_deref(),
            Some("REGEX"),
            "with a query string the prefix no longer spans the request path, so the regex wins",
        );
    }

    /// A rule carrying a matching `method` beats a method-less one in either
    /// declaration order: the matching rule returns immediately, and when the
    /// method-less rule runs first it has only recorded a candidate that the
    /// return then overrides.
    ///
    /// To SEE THIS RED: drop the early `return` from the
    /// `MethodRuleResult::Equals` arm under `PathRuleResult::Regex |
    /// PathRuleResult::Equals` in `Router::lookup` (the same mutation the
    /// matching-method tests name). Both rules then merely record, so the last
    /// declared wins and the first assertion below fails with
    /// `left: Some("ANY-REGEX"), right: Some("GET-REGEX")`.
    #[test]
    fn a_rule_with_a_matching_method_beats_a_method_less_rule_in_either_order() {
        let any = || PathRule::Regex(Regex::new("/a.*").expect("test regex must compile"));
        let getr = || PathRule::Regex(Regex::new("/ab.*").expect("test regex must compile"));

        assert_eq!(
            winner_of(
                (getr(), GET, "GET-REGEX"),
                (any(), NO_METHOD, "ANY-REGEX"),
                "/abc"
            )
            .as_deref(),
            Some("GET-REGEX"),
            "a matching-method rule declared first must win",
        );
        assert_eq!(
            winner_of(
                (any(), NO_METHOD, "ANY-REGEX"),
                (getr(), GET, "GET-REGEX"),
                "/abc"
            )
            .as_deref(),
            Some("GET-REGEX"),
            "a matching-method rule declared second must still win",
        );
    }

    /// A rule whose `method` is PRESENT but does not match the request
    /// (`MethodRuleResult::None`) is skipped outright, so it never competes on
    /// path length at all. Both consequences contradict the unqualified
    /// "longest prefix wins" reading: a LONGER prefix loses to a shorter one
    /// when its method mismatches, and an equal-length tie goes to the FIRST
    /// declared rather than the last, because the later one is skipped before
    /// the `size >= prefix_length` comparison is ever reached.
    ///
    /// To SEE THIS RED: in `Router::lookup`'s path-rule loop, make the
    /// `MethodRuleResult::None` arm under `PathRuleResult::Prefix(size)`
    /// behave like its `MethodRuleResult::All` sibling
    /// (`prefix_length = size; matched = Some((rule, route));`). Non-matching
    /// methods then compete: the first assertion resolves to `LONG-POST` and
    /// the second to `SECOND-POST`.
    #[test]
    fn a_prefix_whose_method_does_not_match_is_skipped_so_it_cannot_win_on_length() {
        assert_eq!(
            winner_of(
                (PathRule::Prefix("/a".to_owned()), GET, "SHORT-GET"),
                (
                    PathRule::Prefix("/ab".to_owned()),
                    WRONG_METHOD,
                    "LONG-POST"
                ),
                "/abc",
            )
            .as_deref(),
            Some("SHORT-GET"),
            "a longer prefix whose method mismatches must not beat a shorter matching one",
        );
        assert_eq!(
            winner_of(
                (PathRule::Prefix("/ab".to_owned()), GET, "FIRST-GET"),
                (
                    PathRule::Prefix("/ab".to_owned()),
                    WRONG_METHOD,
                    "SECOND-POST"
                ),
                "/abc",
            )
            .as_deref(),
            Some("FIRST-GET"),
            "an equal-length tie goes to the FIRST when the later rule's method mismatches",
        );
    }

    /// The same third method value, one tier up: an `EQUALS`/`REGEX` rule whose
    /// method mismatches does not short-circuit and does not even register as a
    /// candidate, so a `PREFIX` that would otherwise lose to it wins.
    ///
    /// To SEE THIS RED: in `Router::lookup`'s path-rule loop, make the
    /// `MethodRuleResult::None` arm under
    /// `PathRuleResult::Regex | PathRuleResult::Equals` behave like its
    /// `MethodRuleResult::All` sibling
    /// (`prefix_length = path_b.len(); matched = Some((rule, route));`). The
    /// mismatching regex then takes the request and the assertion resolves to
    /// `WRONG-METHOD-REGEX`.
    #[test]
    fn a_regex_whose_method_does_not_match_never_beats_a_matching_prefix() {
        assert_eq!(
            winner_of(
                (
                    PathRule::Regex(Regex::new("/a.*").expect("test regex must compile")),
                    WRONG_METHOD,
                    "WRONG-METHOD-REGEX",
                ),
                (PathRule::Prefix("/a".to_owned()), GET, "MATCHING-PREFIX"),
                "/abc",
            )
            .as_deref(),
            Some("MATCHING-PREFIX"),
            "a regex whose method mismatches must not short-circuit the scan",
        );
    }

    /// `path_type = "REGEX"` is NOT anchored. `PathRule::from_config` compiles
    /// the configured value verbatim and `regex::bytes::Regex::is_match` is a
    /// substring search, so a pattern matches anywhere in the request path.
    /// `doc/configure.md` used to claim the `\A...\z` anchoring of v2.0.0
    /// applied here; that work was hostname-segment only. Whether path regexes
    /// SHOULD be anchored is open as sozu#1350 — this test pins today's
    /// behaviour so the answer is a deliberate change, not a silent one.
    ///
    /// The rule is built through `PathRule::from_config`, the path every
    /// configured frontend takes, so anchoring there is what this pins.
    ///
    /// To SEE THIS RED: anchor in `PathRule::from_config` — replace
    /// `Regex::new(&rule.value)` with
    /// `Regex::new(&format!("\\A{}\\z", rule.value))`. The assertion then fails
    /// with `left: None, right: Some("SUBSTRING")`.
    #[test]
    fn a_path_regex_is_unanchored_and_matches_anywhere_in_the_request_path() {
        let rule = PathRule::from_config(CommandPathRule::regex("bc".to_owned()))
            .expect("a valid regex path rule must build");
        let mut router = Router::new();
        assert!(add_path_rule(&mut router, rule, GET, "SUBSTRING"));
        assert_eq!(
            routed_cluster(&router, "/abcd").as_deref(),
            Some("SUBSTRING"),
            "an unanchored path regex must match in the middle of the request path",
        );
    }

    /// One level up from the path rules, two overlapping regex HOSTNAME
    /// segments follow the same first-declared-wins rule: a trie node holds
    /// its regex segments in an ordered list, `lookup` returns on the first
    /// that matches, and a new segment is appended.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_with_path`
    /// (`router/pattern_trie.rs`), iterate the regex segments in reverse —
    /// `for (regexp, child) in self.regexps.iter().rev()`. The last declared
    /// segment then answers and both assertions below flip.
    #[test]
    fn the_first_declared_regex_hostname_segment_wins_over_a_later_overlapping_one() {
        // Both patterns match the segment `test4`.
        let numbered: &[u8] = b"/test[0-9]/.example.com";
        let any_suffixed: &[u8] = b"/[a-z]+[0-9]/.example.com";

        let declare = |first: &[u8], first_cluster: &str, second: &[u8], second_cluster: &str| {
            let mut router = Router::new();
            for (hostname, cluster) in [(first, first_cluster), (second, second_cluster)] {
                assert!(router.add_tree_rule(
                    hostname,
                    &PathRule::Prefix("/".to_owned()),
                    &MethodRule::new(Some("GET".to_owned())),
                    &Route::ClusterId(cluster.to_owned()),
                ));
            }
            router
                .lookup("test4.example.com", "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            declare(numbered, "NUMBERED", any_suffixed, "ANY-SUFFIXED").as_deref(),
            Some("NUMBERED"),
            "the first-declared regex hostname segment must win",
        );
        assert_eq!(
            declare(any_suffixed, "ANY-SUFFIXED", numbered, "NUMBERED").as_deref(),
            Some("ANY-SUFFIXED"),
            "reversing the declaration order reverses the winner",
        );

        // The worked example in `doc/configure.md` promises `/test[0-9]/`
        // matches `test4` "and not testAB". `[0-9]` is a SINGLE digit and the
        // segment is `\A`-`\z` anchored, so `test44` and `xtest4` must miss
        // too — the anchoring is pinned generally by
        // `segment_regex_rejects_partial_matches`, this pins the doc's own
        // example.
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            numbered,
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("REGIONAL".to_owned()),
        ));
        assert_eq!(
            router
                .lookup("test4.example.com", "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
                .as_deref(),
            Some("REGIONAL"),
        );
        for miss in [
            "testAB.example.com",
            "test44.example.com",
            "xtest4.example.com",
        ] {
            assert!(
                router.lookup(miss, "/", &Method::Get).is_err(),
                "{miss} must not match the single-digit anchored segment",
            );
        }
    }

    /// `PathRule`'s `PartialEq` carried no `(Equals, Equals)` arm, so two
    /// identical `PathRule::Equals` compared unequal — an equality that is
    /// not even reflexive. Every router bookkeeping path is written against
    /// it: `add_tree_rule` could not see the rule it had just pushed (so the
    /// same `--path-equals` frontend was stored again on every re-add
    /// instead of being refused), `remove_tree_rule`'s `retain` kept the
    /// entry it was asked to drop, and `remove_http_front` still answered
    /// `Ok` for a route it had not removed.
    ///
    /// To SEE THIS RED: drop the
    /// `(PathRule::Equals(s1), PathRule::Equals(s2)) => s1 == s2` arm from
    /// `impl std::cmp::PartialEq for PathRule` so the catch-all `_ => false`
    /// answers again — the reflexivity assertion below is the first statement
    /// and fails on `left: Equals("/exact") / right: Equals("/exact")`.
    /// Without that assertion a debug build panics one line later, inside
    /// `add_http_front`, on "a freshly inserted tree domain must resolve to
    /// its inserted rule".
    #[test]
    fn an_equals_path_frontend_is_deduplicated_and_removable() {
        assert_eq!(
            PathRule::Equals("/exact".to_owned()),
            PathRule::Equals("/exact".to_owned()),
            "PathRule equality must be reflexive for the Equals variant",
        );

        let mut router = Router::new();
        let mut front = test_http_frontend();
        front.path = CommandPathRule::equals("/exact".to_owned());

        router
            .add_http_front(&front)
            .expect("an Equals frontend must be added");
        assert!(
            router.lookup("example.com", "/exact", &Method::Get).is_ok(),
            "the Equals frontend must resolve once added",
        );

        // The identical frontend is already stored, so the router must
        // refuse it instead of pushing a second, unremovable copy.
        assert!(
            matches!(router.add_http_front(&front), Err(RouterError::AddRoute(_))),
            "re-adding the same Equals frontend must be refused, not duplicated",
        );

        router
            .remove_http_front(&front)
            .expect("an Equals frontend must be removable");
        assert!(
            router
                .lookup("example.com", "/exact", &Method::Get)
                .is_err(),
            "the Equals frontend must no longer resolve once removed",
        );
    }
}
