pub mod pattern_trie;

use std::{
    borrow::Cow,
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
        // The route table stores every hostname LABEL ASCII-lowercased
        // (`add_tree_rule` via `tree_hostname_to_ascii`, and
        // `DomainRule::from_str` via `idna::domain_to_ascii`; a regex
        // segment keeps the operator's bytes on both paths and is
        // compiled case-insensitively instead — see
        // `tree_hostname_to_ascii`), so the lookup key is normalised the
        // same way — otherwise the byte-exact trie walk and the
        // byte-exact `Exact`/`Wildcard` comparisons below can never match
        // an uppercase `Host:` header. See [`normalize_hostname`] for why
        // this is a scan and not an allocation on the common path.
        //
        // The captures handed to `RouteResult` are taken from this
        // normalised key, so `$HOST[n]` rewrite templates see the
        // normalised host; the raw bytes the client sent stay on
        // `HttpContext.authority`, which feeds the access log,
        // `X-Forwarded-Host` and the redirect `Location:` and is not
        // touched here. `RouteNotFound` below deliberately reports the
        // ORIGINAL spelling, because that is the diagnostic an operator
        // needs.
        let normalized_hostname = normalize_hostname(hostname);
        let hostname_b = normalized_hostname.as_bytes();
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

        // The hostname candidates the trie holds are tried
        // most-specific-first — exact name, then regex segments in
        // declaration order, then the `*` wildcard — and `select_tree_rule`
        // is what decides whether a candidate serves THIS request. Handing
        // it to the trie as the acceptance predicate is what makes the
        // order a search rather than a filter: a hostname whose rules all
        // reject this (path, method) hands the request to the next
        // candidate instead of ending the lookup (sozu#1351).
        let trie_path: TrieMatches<'_, '_> = Vec::with_capacity(16);
        if let Some(((_, path_rules), trie_matches)) =
            self.tree
                .lookup_with_path(hostname_b, true, trie_path, &mut |(_, path_rules)| {
                    select_tree_rule(path_rules, path_b, method).is_some()
                })
            && let Some((path_rule, route)) = select_tree_rule(path_rules, path_b, method)
        {
            // The second call cannot disagree with the predicate: same
            // pure function, same leaf, same request. Re-running it is how
            // the winning rule is carried out of a closure that may only
            // answer yes or no.
            //
            // It costs a second scan of the winning leaf's rule list, so
            // the common single-candidate hit now scans twice where it
            // scanned once. The list is the rules of ONE hostname and the
            // scan is a `PathRule::matches` per entry; carrying the
            // selection out of the closure instead would need `accept` to
            // borrow for the trie's own `'b`, which ties the predicate to
            // the tree borrow for a saving of one pass over a short Vec.
            // Revisit if a leaf ever holds enough rules to matter.
            return Ok(RouteResult::new_with_trie(
                hostname_b,
                trie_matches,
                path_b,
                path_rule,
                route,
            ));
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
        //
        // `tags` counts as such a field. The legacy shapes carry no tags
        // (`RouteResult::forward` / `::deny` both set `tags: None`), so a
        // tagged frontend stored as `Route::ClusterId` would hand the mux
        // a tagless routing decision and its access logs would have to
        // fall back to the authority-keyed listener map — the exact
        // spelling mismatch of sozu#1379. Only tagged frontends pay the
        // `Rc<Frontend>`; an untagged one keeps the lightweight shape.
        let has_policy = front.redirect.is_some()
            || front.redirect_scheme.is_some()
            || front.redirect_template.is_some()
            || front.rewrite_host.is_some()
            || front.rewrite_path.is_some()
            || front.rewrite_port.is_some()
            || front.required_auth.unwrap_or(false)
            || !front.headers.is_empty()
            || front.hsts.is_some()
            || front.tags.is_some();

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

        match tree_hostname_to_ascii(hostname) {
            Some(hostname) => {
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
            None => false,
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

        match tree_hostname_to_ascii(hostname) {
            Some(hostname) => {
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
            None => false,
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
        // Same normalisation as `lookup`, for the same reason: the
        // pre/post arms compare byte-exact against `DomainRule`s the add
        // path already lowercased. Answering `false` for a hostname whose
        // route is still installed would drop the frontend's cached tags
        // from under a live route.
        let normalized_hostname = normalize_hostname(hostname);
        let hostname = normalized_hostname.as_ref();
        let hostname_b = hostname.as_bytes();

        // Check pre rules
        for (domain_rule, _, _, _) in &self.pre {
            if domain_rule.matches(hostname_b) {
                return true;
            }
        }

        // Check tree rules (exact match only, no wildcard resolution)
        if let Some(ascii_hostname) = tree_hostname_to_ascii(hostname)
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

/// Pick the rule a trie leaf serves `path` and `method` with, or `None`
/// when the leaf carries nothing for this request.
///
/// An `EQUALS`/`REGEX` rule whose `method` matches exactly wins
/// immediately; otherwise the longest matching `PREFIX` wins, with a
/// method-less rule competing on equal footing. Rules whose `method` is
/// set and does not match are skipped entirely — which is why a longer
/// prefix can lose to a shorter one (`doc/configure.md`, "Path matching
/// precedence within a frontend").
///
/// Lifted out of [`Router::lookup`] so the trie walk can use the very
/// same selection as its acceptance predicate: a hostname candidate that
/// selects nothing must not end the lookup (sozu#1351). Two copies of
/// this would drift, and the drift would be a route that the trie
/// accepted and the router then refused to serve.
fn select_tree_rule<'a>(
    path_rules: &'a [(PathRule, MethodRule, Route)],
    path: &[u8],
    method: &Method,
) -> Option<(&'a PathRule, &'a Route)> {
    let mut prefix_length = 0;
    let mut matched: Option<(&PathRule, &Route)> = None;

    for (rule, method_rule, route) in path_rules {
        match rule.matches(path) {
            PathRuleResult::Regex | PathRuleResult::Equals => match method_rule.matches(method) {
                MethodRuleResult::Equals => return Some((rule, route)),
                MethodRuleResult::All => {
                    prefix_length = path.len();
                    matched = Some((rule, route));
                }
                MethodRuleResult::None => {}
            },
            PathRuleResult::Prefix(size) => {
                if size >= prefix_length {
                    match method_rule.matches(method) {
                        // FIXME: the rule order will be important here
                        MethodRuleResult::Equals | MethodRuleResult::All => {
                            // Longest-prefix wins: the selected length is
                            // monotonically non-decreasing across the
                            // candidate scan.
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

    matched
}

/// ASCII-lowercase a hostname for routing, borrowing it when it already is.
///
/// RFC 9110 §4.2.3 makes the host case-insensitive, and every configured
/// hostname LABEL is lowercased on the way into the route table:
/// [`Router::add_tree_rule`] runs it through [`tree_hostname_to_ascii`]
/// and [`DomainRule::from_str`] through `idna::domain_to_ascii`. (A
/// `/`-delimited regex segment is exempt on both paths and is folded at
/// compile time instead; see [`tree_hostname_to_ascii`].) The lookup key
/// has to be normalised
/// the same way, or [`Router::lookup`]'s byte-exact trie walk and the
/// byte-exact `DomainRule::Exact` / `DomainRule::Wildcard` comparisons
/// can never match an uppercase `Host:` header — and no configuration
/// fixes it, because declaring the frontend in uppercase gets lowercased
/// too.
///
/// The scan is the whole cost on the common path: a host that is already
/// lowercase — every host a conforming client sends — is borrowed, so
/// the datapath allocates only for the requests that actually carry
/// uppercase. This runs once per request, so the distinction is not
/// academic.
///
/// Deliberately ASCII-only and deliberately NOT a full
/// `idna::domain_to_ascii` round trip: `hostname_and_port`
/// (`lib/src/protocol/kawa_h1/parser.rs`) only admits bytes matching
/// `is_hostname_char`, all of which are ASCII, so there is no non-ASCII
/// host to punycode here, and a per-request IDNA pass would be a real
/// datapath cost for a case that cannot occur.
fn normalize_hostname(hostname: &str) -> Cow<'_, str> {
    if hostname.as_bytes().iter().any(u8::is_ascii_uppercase) {
        Cow::Owned(hostname.to_ascii_lowercase())
    } else {
        Cow::Borrowed(hostname)
    }
}

/// Normalise a `RulePosition::Tree` hostname for the route table,
/// applying IDNA to its literal labels ONLY.
///
/// `idna::domain_to_ascii` ASCII-lowercases everything it is handed, and
/// a tree hostname is not all domain labels: a `/`-delimited segment is
/// regex SOURCE, where case is meaning. Folding it inverts every
/// uppercase escape — measured, `"/\D+/.example.com"` normalises to
/// `"/\d+/.example.com"` and `"/[^\D]/.example.com"` to
/// `"/[^\d]/.example.com"`, turning "not a digit" into "a digit", so the
/// installed rule matched the exact COMPLEMENT of what the operator
/// wrote while `sozu query frontends` still echoed the original spelling
/// (sozu#1377). `\W`/`\w` and `\S`/`\s` invert the same way.
///
/// The split is the one [`convert_regex_domain_rule`] already makes for
/// `Pre`/`Post` and the one `pattern_trie`'s `insert_recursive`
/// addresses a `regexps` entry by: a label the operator wrapped in
/// slashes is a regex segment and is copied BYTE FOR BYTE, delimiters
/// included; every other label is a domain label and goes through
/// `idna::domain_to_ascii` exactly as before. Per-label and whole-domain
/// IDNA agree on a label — measured, `"MÜNCHEN"` maps to
/// `"xn--mnchen-3ya"` either way — so a genuine unicode label is still
/// punycoded in the very same hostname whose regex segment is left
/// alone.
///
/// Leaving the source unfolded means an uppercase LITERAL inside a
/// segment (`/API[0-9]/`) no longer meets an ASCII-lowercased lookup key
/// by accident. `pattern_trie`'s `compiled_segment` compiles the stored
/// segment case-insensitively instead, which is what
/// [`DomainRule::from_str`] already does for `Pre`/`Post`: the two
/// positions now treat a hostname regex identically rather than by
/// opposite mechanisms.
///
/// Two inputs deliberately keep the historical whole-string call, both
/// byte-for-byte:
///
/// - a hostname with no `/` at all, which is every ordinary hostname —
///   so the retained trailing dot pinned by
///   `an_absolute_form_host_does_not_reach_a_relative_frontend_yet` and
///   every other whole-string behaviour is untouched;
/// - a hostname carrying a `/` that does NOT parse as this grammar, such
///   as `abc/[0-9]+/.example.com` (a segment that is not `.`-anchored) or
///   `example.com/`. Neither the split nor the trie sees a regex segment
///   there — it is one literal label to both — so there is nothing to
///   protect, and the trie accepts or rejects it exactly as it did
///   before.
fn tree_hostname_to_ascii(hostname: &str) -> Option<String> {
    /// The `/…/`-aware walk. `None` means "not this grammar", never
    /// "invalid": validating the segment is `anchored_segment`'s job at
    /// insert time, and this function must not start rejecting hostnames
    /// the trie has always answered for itself.
    fn split_labels(hostname: &str) -> Option<String> {
        let mut result = String::with_capacity(hostname.len());
        let s = hostname.as_bytes();
        let mut index = 0;

        loop {
            // A bare trailing `.` leaves `index` one past the end; the
            // grammar requires a label after every `.`, so this is not
            // the shape — fall back rather than index out of bounds.
            if index >= s.len() {
                return None;
            }

            if s[index] == b'/' {
                let close = (index + 1..s.len()).find(|&i| s[i] == b'/')?;
                // Regex source, copied verbatim. Both delimiters are
                // ASCII `/`, so the slice is on char boundaries.
                result.push_str(std::str::from_utf8(&s[index..=close]).ok()?);
                index = close + 1;
            } else {
                let end = (index..s.len()).find(|&i| s[i] == b'.').unwrap_or(s.len());
                let label = std::str::from_utf8(&s[index..end]).ok()?;
                result.push_str(&::idna::domain_to_ascii(label).ok()?);
                index = end;
            }

            if index == s.len() {
                return Some(result);
            }
            if s[index] != b'.' {
                return None;
            }
            result.push('.');
            index += 1;
        }
    }

    if hostname.contains('/')
        && let Some(normalized) = split_labels(hostname)
    {
        return Some(normalized);
    }
    ::idna::domain_to_ascii(hostname).ok()
}

#[derive(Clone, Debug)]
pub enum DomainRule {
    Any,
    Exact(String),
    /// Matches when `hostname` ends with `s[1..]` (the wildcard pattern with
    /// the leading `*` stripped) and the remaining leftmost prefix is
    /// non-empty and contains no `.`. Comparison is byte-exact and no
    /// IDN/punycode normalisation is performed here: the pattern was
    /// already lowercased by `DomainRule::from_str`'s
    /// `idna::domain_to_ascii`, and the hostname it is handed has been
    /// through `normalize_hostname` in [`Router::lookup`], so both
    /// sides are ASCII-lowercase by the time they meet. Stored with the
    /// leading `*`.
    Wildcard(String),
    /// Anchored full-host regex built by `convert_regex_domain_rule`.
    /// Unlike the other variants its source is NOT lowercased — it is
    /// compiled case-insensitively instead, so an uppercase literal
    /// still matches the normalised key. See `DomainRule::from_str`
    /// for why that folding is Unicode-aware while the hostnames it is
    /// ever handed are ASCII.
    Regex(Regex),
}

/// Build the whole-host regex for a `hostname` carrying one or more
/// slash-delimited regex segments (`/cdn[0-9]+/.example.com`). Each regex
/// segment is emitted as a non-capturing group, each literal label is
/// emitted through [`regex::escape`], the `.` separators are escaped, and the
/// result is anchored at both ends.
///
/// The anchors and the groups answer two different failures and both are
/// required:
///
/// - `\A` … `\z` because `Regex::is_match` is a substring search. Without
///   them `/example\.com/` matches any hostname CONTAINING `example.com`
///   (e.g. `attacker.example.com.evil.org`), letting an attacker-controlled
///   domain reach a frontend that should only serve `example.com`.
/// - `(?:` … `)` around each regex segment because `|` binds looser than
///   concatenation, so an ungrouped segment splits the anchors between its
///   branches: `\Aa|b\.example\.com\z` parses as
///   `(\Aa)|(b\.example\.com\z)`. Measured, that matches
///   `axx.example.com` and `xxb.example.com`; with three branches the middle
///   one keeps NEITHER anchor and matches anywhere at all. The group is
///   non-capturing, so `captures_len` — and therefore every `$HOST[n]`
///   rewrite index, read from `caps.iter().skip(1)` in
///   `RouteResult::new_no_trie` — is unchanged. A literal label carrying a
///   `|` split the anchors the same way and is closed by the escaping
///   below: `/a/.x|y.com` assembled to `\A(?:a)\.x|y\.com\z`, which
///   matched `a.xZZZ` and `ZZZy.com`.
///
/// A label the operator did NOT wrap in slashes is a literal, and the two
/// arms must not be confused: grouping it would confine an alternation but
/// leave `+`, `?`, `*`, `^` and `$` live inside it, so `b+c` would still
/// match `bbbc`. [`regex::escape`] is the treatment that matches the
/// grammar. It is a no-op in matching behaviour for every label a real
/// hostname can carry — an LDH label is `[A-Za-z0-9-]`, of which `escape`
/// touches only `-`, and `\-` is `-` outside a character class. Two
/// deliberate behaviour changes fall out of it for labels that are NOT
/// valid hostname labels, both narrowing and both measured:
/// `*./x/.example.com` assembled to `\A*\.(?:x)\.example\.com\z`, whose
/// `\A*` is a repetition of a zero-width assertion and therefore matches
/// empty anywhere — it took `evil.attacker.x.example.com`, and now requires
/// the literal label `*`; and `/a/.b(c.example.com` was REJECTED because the
/// assembly did not compile, and is now accepted, matching exactly the
/// literal host the operator typed and nothing else.
///
/// The `*` change is scoped to what this function feeds, which is `Pre` and
/// `Post` matching only. A trie-routed frontend never reaches here for
/// matching: `TrieNode` splits the hostname on `.` and keeps a `*` label in
/// its own `wildcard` slot, so the same configured hostname now answers
/// differently by position. `doc/configure.md` § "Regex hostname segments"
/// carries the table and
/// `a_star_label_is_a_wildcard_on_the_trie_and_a_literal_on_pre_and_post`
/// asserts it.
///
/// Each regex segment is compiled ON ITS OWN before it is wrapped. The
/// wrapper supplies one `(` and one `)`, so an unbalanced segment can be
/// balanced BY the wrapping: `a)(b` is rejected by `Regex::new`, yet
/// `\A(?:a)(b)\.example\.com\z` compiles and matches `ab.example.com`.
/// Without that first compile, grouping would silently promote a hostname the
/// router rejects today into a live rule. This closes
/// invalid-becomes-valid only; the converse is not closed, exactly as on the
/// path side — see [`PathRule::anchored_regex`], which carries the same shape
/// and the same measurements for a whole path value.
fn convert_regex_domain_rule(hostname: &str) -> Option<String> {
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
                        Ok(r) => {
                            // Reject a segment that is not a regex on its own,
                            // then group it. See this function's doc comment.
                            Regex::new(r).ok()?;
                            result.push_str("(?:");
                            result.push_str(r);
                            result.push(')');
                        }
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
                        // A LITERAL label is not a pattern: escape it so it
                        // matches itself. See this function's doc comment.
                        Ok(r) => result.push_str(&regex::escape(r)),
                        Err(_) => return None,
                    }
                    break;
                }
            }
            if index == s.len() {
                match std::str::from_utf8(&s[start..]) {
                    Ok(r) => result.push_str(&regex::escape(r)),
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
                // Case-insensitive, because `Router::lookup`
                // normalises the host it matches against and a hostname
                // is case-insensitive anyway (RFC 9110 §4.2.3). Without
                // it an operator's `/API[0-9]/` would compile
                // case-sensitively and match nothing at all — it used to
                // match `API7.…` only, never the canonical `api7.…`.
                //
                // Set on the builder rather than by lowercasing the
                // source, which would rewrite `\D` into `\d`. The trie
                // does exactly the same since sozu#1377:
                // `tree_hostname_to_ascii` leaves a segment's bytes alone
                // and `pattern_trie::compiled_segment` folds at compile
                // time, so the two rule positions now treat a hostname
                // regex by the same mechanism. Up to and including 2.2.1
                // `RulePosition::Tree` instead ran the WHOLE hostname,
                // regex source included, through `idna::domain_to_ascii`
                // — which is precisely the escape-inverting fold this
                // comment refuses.
                //
                // The folding is the crate default, i.e. Unicode simple
                // case folding, NOT ASCII-only: `.unicode(false)` is
                // deliberately left unset. It would make the fold
                // ASCII-only, but it also rejects patterns that compile
                // and work today — measured, `\p{L}` and `\pL` fail to
                // build with it, so `/\p{L}+/.example.com`, which
                // matches ASCII letters perfectly well right now, would
                // start being refused at add time — pinned by
                // `a_unicode_class_hostname_regex_still_compiles_and_routes`.
                // The distinction is
                // unobservable on the haystack side regardless: the only
                // hostnames that reach `matches` come through
                // `hostname_and_port`
                // (`lib/src/protocol/kawa_h1/parser.rs`), whose
                // `is_hostname_char` admits nothing outside ASCII, and
                // whose own `if !i.is_empty()` guard then refuses the
                // whole authority rather than truncating it — see
                // `is_hostname_char_admits_only_ascii`. It IS observable
                // on the PATTERN side: a pattern spelling `\u{212A}`
                // folds onto `k`. Hence "case-insensitive" here, not
                // "ASCII-case-insensitive".
                Some(s) => match regex::bytes::RegexBuilder::new(&s)
                    .case_insensitive(true)
                    .build()
                {
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

    /// Compile a configured `path_type = "REGEX"` value anchored at both ends,
    /// so it matches the WHOLE request path and nothing else — the behaviour
    /// `doc/configure.md` has documented since v2.0.0 and the code did not
    /// implement (sozu#1350). `regex::bytes::Regex::is_match` is a substring
    /// search, so the previous bare `Regex::new(&rule.value)` let `bc` match
    /// `/abcd`.
    ///
    /// `\A(?:` … `)\z` is the form the hostname side uses too, in
    /// [`convert_regex_domain_rule`] around each slash-delimited segment and
    /// in `pattern_trie.rs`'s `anchored_segment` at segment-insert time
    /// (sozu#1356 closed the hostname half; this path half landed first, as
    /// sozu#1357).
    ///
    /// The non-capturing group is NOT decoration: `|` binds looser than
    /// concatenation, so the ungrouped `\Aa|b\z` parses as `(\Aa)|(b\z)`
    /// and each branch keeps only one anchor — measured, it still matches
    /// `axx`. `\A(?:a|b)\z` does not. `(?:` … `)` is non-capturing, so
    /// `captures_len` and therefore every `$PATH[n]` rewrite index are
    /// unchanged.
    ///
    /// The configured value is compiled ON ITS OWN first and only a value
    /// that compiles is wrapped. The wrapper supplies one `(` and one `)`, so
    /// an unbalanced pattern can be balanced BY the wrapping: `a)(b` is
    /// rejected by `Regex::new`, yet `\A(?:a)(b)\z` compiles and matches
    /// `ab`. Without the first compile, anchoring would silently promote a
    /// rule the router rejects today into a live rule matching something the
    /// operator never wrote.
    ///
    /// This closes invalid-becomes-valid only. The opposite direction is NOT
    /// closed and is not claimed to be: a pattern that compiles bare can be
    /// rejected once wrapped, because the appended `)\z` has to survive
    /// whatever the pattern left open. Measured: `(?x)/api/v1 # v1 only`
    /// compiles bare and fails wrapped with `unclosed group` — `(?x)` enables
    /// `#` comments, and the appended `)` lands inside one — and a 249-deep
    /// nested group compiles bare and fails wrapped against the crate's
    /// 250-nesting limit. Both land in the `None` arm of `from_config` and
    /// surface as [`RouterError::InvalidPathRule`] at frontend registration,
    /// which `bin/src/command/requests.rs`'s `validate_frontend_request` runs
    /// through this very same `add_http_front` before fanning out: the
    /// frontend is refused loudly and identically in main and worker, never
    /// silently mis-routed.
    ///
    /// Anchors the operator wrote by hand are harmless and are left alone:
    /// `regex`'s `^`/`$` are `\A`/`\z` in the default (non-multi-line) mode —
    /// `$` does not match before a trailing `\n` — so the repeated zero-width
    /// assertions in `\A(?:^/abc$)\z` collapse. Measured: that pattern matches
    /// `/abc` and not `/xabcd`, exactly as `\A(?:/abc)\z` does.
    fn anchored_regex(value: &str) -> Option<Regex> {
        Regex::new(value).ok()?;
        Regex::new(&format!("\\A(?:{value})\\z")).ok()
    }

    pub fn from_config(rule: CommandPathRule) -> Option<Self> {
        match PathRuleKind::try_from(rule.kind) {
            Ok(PathRuleKind::Prefix) => Some(PathRule::Prefix(rule.value)),
            Ok(PathRuleKind::Regex) => Self::anchored_regex(&rule.value).map(PathRule::Regex),
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
/// preload-list convention (<https://hstspreload.org/>).
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

/// A pre-parsed rewrite template, decomposed into `RewritePart`s.
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
    use quickcheck::{Arbitrary, Gen, TestResult, quickcheck};

    /// A frontend's `method` is classified by the same case-sensitive
    /// [`Method::new`] as the request's (sozu-proxy/sozu#1451), so the
    /// operator-facing rule is "declare the method in the exact case the
    /// client sends". A frontend declared `method = "get"` is a custom-method
    /// rule: it no longer answers a canonical `GET` request, and it answers a
    /// literal `get` one instead.
    #[test]
    fn method_rule_is_case_sensitive() {
        let declared_lowercase = MethodRule::new(Some("get".to_owned()));

        assert_eq!(
            declared_lowercase.inner,
            Some(Method::Custom("get".to_owned())),
            "a lowercase declaration must not collapse onto Method::Get"
        );
        assert!(
            declared_lowercase.matches(&Method::Get) == MethodRuleResult::None,
            "a lowercase declaration must not match a canonical GET request"
        );
        assert!(
            declared_lowercase.matches(&Method::new(b"get")) == MethodRuleResult::Equals,
            "a lowercase declaration must match a literal lowercase request"
        );

        let declared_canonical = MethodRule::new(Some("GET".to_owned()));

        assert!(
            declared_canonical.matches(&Method::Get) == MethodRuleResult::Equals,
            "a canonical declaration must still match a canonical GET request"
        );
        assert!(
            declared_canonical.matches(&Method::new(b"get")) == MethodRuleResult::None,
            "a canonical declaration must not match a lowercase request"
        );
    }

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
        // (unanchored by default) only succeeds on a full-host match, and each
        // slash-delimited regex segment is wrapped in a non-capturing group so
        // an alternation inside it cannot split those anchors between its
        // branches (sozu#1356). A literal segment is not a pattern and is not
        // grouped.
        assert_eq!(
            convert_regex_domain_rule("www.example.com")
                .unwrap()
                .as_str(),
            "\\Awww\\.example\\.com\\z"
        );
        // A `*` label reaches this function only when the hostname ALSO
        // carries a `/` segment, because `DomainRule::from_str` tests
        // `contains('/')` first — these two rows exercise the function
        // directly. `*` is a LITERAL label here, not a wildcard, and is
        // escaped as one. Unescaped it was a regex quantifier over the
        // preceding atom: `\A*` repeats a zero-width assertion, so it
        // matched empty at any offset and left the whole pattern unanchored
        // at its start.
        assert_eq!(
            convert_regex_domain_rule("*.example.com").unwrap().as_str(),
            "\\A\\*\\.example\\.com\\z"
        );
        assert_eq!(
            convert_regex_domain_rule("test.*.example.com")
                .unwrap()
                .as_str(),
            "\\Atest\\.\\*\\.example\\.com\\z"
        );
        assert_eq!(
            convert_regex_domain_rule("css./cdn[a-z0-9]+/.example.com")
                .unwrap()
                .as_str(),
            "\\Acss\\.(?:cdn[a-z0-9]+)\\.example\\.com\\z"
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
        assert_eq!(pattern.as_str(), "\\A(?:seg1)\\.foo\\.(?:seg2)\\.com\\z");
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
            DomainRule::Regex(Regex::new("\\A(?:cdn[0-9]+)\\.example\\.com\\z").unwrap())
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

    /// Declare one regex path rule built the way a configured frontend builds
    /// it — through `PathRule::from_config`, which is where the anchoring
    /// lives — and resolve `path`.
    fn regex_rule_routes(pattern: &str, path: &str) -> Option<String> {
        let rule = PathRule::from_config(CommandPathRule::regex(pattern.to_owned()))
            .unwrap_or_else(|| panic!("the regex path rule {pattern:?} must build"));
        let mut router = Router::new();
        assert!(add_path_rule(&mut router, rule, GET, "REGEX"));
        routed_cluster(&router, path)
    }

    /// Resolve a table of `(pattern, request path, must route)` triples
    /// through [`regex_rule_routes`]. Each row names itself in the failure
    /// message, so a red run says which triple broke rather than which
    /// assertion index did.
    fn assert_regex_routing(cases: &[(&str, &str, bool)]) {
        let verdict = |routes: bool| if routes { "match" } else { "no match" };
        for &(pattern, path, must_route) in cases {
            let routes = regex_rule_routes(pattern, path).is_some();
            assert_eq!(
                routes,
                must_route,
                "path regex {pattern:?} against request path {path:?}: \
                 expected {}, measured {}",
                verdict(must_route),
                verdict(routes),
            );
        }
    }

    /// `path_type = "REGEX"` is anchored at both ends: the configured pattern
    /// must match the WHOLE request path, not a substring of it.
    ///
    /// This test is the INVERSION of the test formerly named
    /// `a_path_regex_is_unanchored_and_matches_anywhere_in_the_request_path`
    /// — a name no longer in the tree, this one having replaced it —
    /// added by #1352 to pin the opposite. That test was correct about the
    /// code and is now deliberately obsolete: sozu#1350 asked whether
    /// `doc/configure.md` (which has promised `\A...\z` since v2.0.0) or
    /// `PathRule::from_config` (a bare `Regex::new` against the substring
    /// search `regex::bytes::Regex::is_match`) was the thing to change, and
    /// the answer was the code. The old test's own comment said it existed so
    /// that the answer would be "a deliberate change, not a silent one" —
    /// this is that deliberate change, kept visible by rewriting the test
    /// rather than deleting it.
    ///
    /// `bc` matching `/abcd` is sozu#1350's own example, so it is the case
    /// kept here.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. The first assertion then fails
    /// with `left: Some("REGEX"), right: None`.
    #[test]
    fn a_path_regex_is_anchored_at_both_ends_and_must_match_the_whole_request_path() {
        assert_eq!(
            regex_rule_routes("bc", "/abcd"),
            None,
            "an anchored path regex must not match in the middle of the request path",
        );
        assert_eq!(
            regex_rule_routes("/ab[cd]", "/abc").as_deref(),
            Some("REGEX"),
            "a regex spanning the whole request path must still match",
        );
        assert_eq!(
            regex_rule_routes("/ab[cd]", "/abcd"),
            None,
            "the trailing `\\z` must reject a longer path with a matching head",
        );
        assert_eq!(
            regex_rule_routes("/ab[cd]", "/x/abc"),
            None,
            "the leading `\\A` must reject a path with a matching tail",
        );

        // The matched "request path" is the request-target as it arrives,
        // query string included, so anchoring bites a query string the way it
        // already bit `path_type = "EQUALS"` — the consequence
        // `doc/configure.md` now states.
        assert_eq!(
            regex_rule_routes("/ab[cd]", "/abc?x=1"),
            None,
            "an anchored regex must not match a path carrying a query string \
             unless the pattern accounts for it",
        );
        assert_eq!(
            regex_rule_routes("/ab[cd].*", "/abc?x=1").as_deref(),
            Some("REGEX"),
            "appending `.*` is what makes such a rule survive a query string",
        );
    }

    /// The narrowing reaches the degenerate pattern too: an empty `REGEX`
    /// value compiles (it always has) and used to match every request path,
    /// because an empty match at offset 0 is a match. Anchored, it matches the
    /// empty path and nothing else. An operator using `path_type = "REGEX"`
    /// with an empty `path` as a catch-all loses every request to it.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. The assertion then fails with
    /// `left: Some("REGEX"), right: None`.
    #[test]
    fn an_empty_path_regex_no_longer_matches_every_request_path() {
        assert!(
            Regex::new("").is_ok(),
            "the empty pattern must stay a VALID regex — this test is about \
             what it matches, not about rejecting it",
        );
        assert_eq!(
            regex_rule_routes("", "/abcd"),
            None,
            "an anchored empty path regex must not match a non-empty path",
        );
        // "matched every path", measured against the BARE regex of 2.2.1 — a
        // control. An empty match at offset 0 is a match, so `is_match`
        // succeeded on every input; that is what made an empty `REGEX` value
        // usable as a catch-all in the first place.
        let bare_empty = Regex::new("").expect("the empty pattern compiles");
        for every_path in ["/anything", "/abcd", "/", ""] {
            assert!(
                bare_empty.is_match(every_path.as_bytes()),
                "up to 2.2.1 the unwrapped empty pattern matched {every_path:?}, \
                 as it matched every path",
            );
        }

        assert_eq!(
            regex_rule_routes("", "").as_deref(),
            Some("REGEX"),
            "\"matches only the empty path\" is half a claim without the path \
             it does still match",
        );
    }

    /// Anchors an operator wrote by hand keep working, and are neither
    /// detected nor stripped: `regex`'s `^`/`$` ARE `\A`/`\z` outside
    /// multi-line mode, so the repeated zero-width assertions in
    /// `\A(?:^/abc$)\z` collapse to one at each end. Measured rather than
    /// assumed — that is what the pattern-string assertion below pins.
    ///
    /// The third case is the one that changes: a HALF-anchored pattern
    /// (`^/abc`, start only) used to match `/abcd` and no longer does. An
    /// operator who wrote the leading anchor on the strength of the old
    /// unanchored behaviour gets the missing end supplied for them.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. The half-anchored case then
    /// fails with `left: Some("REGEX"), right: None`, and the pattern-string
    /// assertion fails too. To see ONLY the pattern-string assertion red,
    /// strip a leading `^`/`\A` and a trailing `$`/`\z` from `value` before
    /// wrapping it.
    #[test]
    fn a_hand_written_path_anchor_is_kept_and_double_anchoring_is_harmless() {
        for hand_anchored in ["^/abc$", "\\A/abc\\z", "^/abc"] {
            assert_eq!(
                regex_rule_routes(hand_anchored, "/abc").as_deref(),
                Some("REGEX"),
                "{hand_anchored} must still match the path it was written for",
            );
            assert_eq!(
                regex_rule_routes(hand_anchored, "/abcd"),
                None,
                "{hand_anchored} must not match a longer path",
            );
            assert_eq!(
                regex_rule_routes(hand_anchored, "/x/abc"),
                None,
                "{hand_anchored} must not match a path with a matching tail",
            );
        }

        assert_eq!(
            PathRule::anchored_regex("^/abc$")
                .expect("a hand-anchored pattern must compile")
                .as_str(),
            "\\A(?:^/abc$)\\z",
            "the configured value is wrapped verbatim; no anchor is stripped",
        );

        // `CHANGELOG.md` and `PathRule::anchored_regex` both quote `/xabcd`
        // as the path `\A(?:^/abc$)\z` must not match. It is a positive
        // control rather than a reddenable row — `^/abc$` excludes it with or
        // without the wrapping — but the quoted example was asserted nowhere.
        assert_eq!(
            regex_rule_routes("^/abc$", "/xabcd"),
            None,
            "the example `CHANGELOG.md` quotes verbatim must hold",
        );

        // The load-bearing half of "`^`/`$` ARE `\A`/`\z`": in the `regex`
        // crate's default mode `$` is `\z` and NOT `\Z`, so it does not
        // match before a trailing `\n`. Were it `\Z`, the claim that an
        // already-anchored `^/abc$` is unchanged by the wrapping would be
        // false for exactly one input — a path ending in a newline. Whether a
        // client can drive such a request-target past the parser is NOT
        // established here, and deliberately so: the wrapping claim has to
        // hold on the bytes the router is handed, whatever produced them.
        // Measured against the crate directly, so it is a control.
        assert!(
            !Regex::new("^/abc$")
                .expect("the hand-anchored pattern compiles")
                .is_match(b"/abc\n"),
            "`$` must be `\\z` and not `\\Z`, or wrapping a hand-anchored \
             pattern would not be the no-op this test claims",
        );
        assert_eq!(
            regex_rule_routes("^/abc$", "/abc\n"),
            None,
            "and the wrapped rule must agree with it",
        );
    }

    /// The anchoring wraps the configured value in a NON-CAPTURING GROUP, and
    /// that group is load-bearing: `|` binds looser than concatenation, so the
    /// ungrouped `\A/a|/b\z` parses as `(\A/a)|(/b\z)` and leaves each branch
    /// anchored at one end only. `/a` would still match `/axx`.
    ///
    /// Two branches only ever exercise the FIRST and LAST branch, so the
    /// three-branch case below is what makes `doc/configure.md`'s "every
    /// branch […] not just the first and last" a testable sentence: ungrouped,
    /// a MIDDLE branch keeps neither anchor and matches as a bare substring
    /// anywhere in the path — worse than the half-anchored ends.
    ///
    /// The hostname sites carried the same gap when this landed and were
    /// closed separately by sozu#1356: `convert_regex_domain_rule` now
    /// groups each slash-delimited segment, and `pattern_trie.rs`'s
    /// `anchored_segment` builds `\A(?:{segment})\z` for the regex fed to
    /// `regexp.is_match(segment)`. See
    /// `every_branch_of_an_alternating_regex_hostname_rule_is_anchored` and
    /// `an_alternating_regex_hostname_segment_is_anchored_on_every_branch`.
    ///
    /// To SEE THIS RED: drop the group in `PathRule::anchored_regex` — wrap as
    /// `format!("\\A{value}\\z")`, the ungrouped hostname form. The `/axx`
    /// assertion then fails with `left: Some("REGEX"), right: None`; with the
    /// two-branch rows removed, the three-branch table fails on
    /// `"/a|/b|/c"` against `"zzz/bzzz"` with `left: true, right: false`, the
    /// middle branch matching in the middle of the path.
    #[test]
    fn every_branch_of_an_alternating_path_regex_is_anchored() {
        for whole in ["/a", "/b"] {
            assert_eq!(
                regex_rule_routes("/a|/b", whole).as_deref(),
                Some("REGEX"),
                "{whole} is a whole-path match for one branch and must route",
            );
        }
        assert_eq!(
            regex_rule_routes("/a|/b", "/axx"),
            None,
            "the first branch must be anchored at its END too",
        );
        assert_eq!(
            regex_rule_routes("/a|/b", "/xx/b"),
            None,
            "the last branch must be anchored at its START too",
        );
        assert_eq!(
            PathRule::anchored_regex("/a|/b")
                .expect("an alternation must compile")
                .captures_len(),
            1,
            "the wrapping group must be non-capturing, so `$PATH[n]` indices \
             are untouched",
        );

        // The exact wrapper string, so "grouped" is asserted and not merely
        // implied by the three routing verdicts above.
        assert_eq!(
            PathRule::anchored_regex("/a|/b")
                .expect("an alternation must compile")
                .as_str(),
            "\\A(?:/a|/b)\\z",
            "the wrapper must be the NON-capturing group form",
        );

        // The control for all of the above, built by hand against the `regex`
        // crate rather than through `anchored_regex`: the UNGROUPED wrapper —
        // the form the hostname sites use — parses as `(\A/a)|(/b\z)` and
        // still matches `/axx`. No mutation of `anchored_regex` can move this
        // assertion, which is the point: it measures why the group is there.
        let ungrouped = Regex::new("\\A/a|/b\\z").expect("the ungrouped wrapper compiles");
        assert!(
            ungrouped.is_match(b"/axx"),
            "{} leaves the first branch anchored at its start only",
            ungrouped.as_str(),
        );
        assert!(
            ungrouped.is_match(b"/xx/b"),
            "{} leaves the last branch anchored at its end only",
            ungrouped.as_str(),
        );

        // Two branches cannot show what `doc/configure.md` actually claims —
        // "every branch of an alternation is anchored, NOT JUST THE FIRST AND
        // LAST" — because with two branches there is no other kind. A third
        // branch has one, and it is strictly worse than the half-anchored
        // ends above: ungrouped, the MIDDLE branch keeps NEITHER anchor, so
        // `/b` matches as a bare substring anywhere in the path. Control,
        // measured against the crate.
        let three = Regex::new("\\A/a|/b|/c\\z").expect("the ungrouped wrapper compiles");
        for anywhere in ["zzz/bzzz", "/xx/b/yy", "xx/byy"] {
            assert!(
                three.is_match(anywhere.as_bytes()),
                "{} leaves the MIDDLE branch unanchored at BOTH ends, so it \
                 matches {anywhere} anywhere in the path",
                three.as_str(),
            );
        }

        // The grouped form the router actually builds refuses all three, and
        // still matches each branch whole. This half IS production.
        assert_regex_routing(&[
            ("/a|/b|/c", "/a", true),
            ("/a|/b|/c", "/b", true),
            ("/a|/b|/c", "/c", true),
            ("/a|/b|/c", "zzz/bzzz", false),
            ("/a|/b|/c", "/xx/b/yy", false),
            ("/a|/b|/c", "xx/byy", false),
            ("/a|/b|/c", "/axx", false),
            ("/a|/b|/c", "/xx/c", false),
        ]);
        assert_eq!(
            PathRule::anchored_regex("/a|/b|/c")
                .expect("a three-branch alternation must compile")
                .as_str(),
            "\\A(?:/a|/b|/c)\\z",
            "a middle branch needs the group as much as the outer two do",
        );
    }

    /// Anchoring must not rescue a pattern the router rejects today. The
    /// wrapper supplies one `(` and one `)`, so an unbalanced value can be
    /// balanced BY the wrapping — `a)(b` is not a regex, yet `\A(?:a)(b)\z`
    /// is, and it matches `ab`. `PathRule::anchored_regex` therefore compiles
    /// the configured value on its own before wrapping it.
    ///
    /// This test covers that one direction. The converse is NOT guarded and
    /// is not asserted here: wrapping can reject a pattern that compiles
    /// bare, measured on `(?x)/api/v1 # v1 only` (the appended `)` lands in
    /// the `#` comment `(?x)` enables) and on a 249-deep nested group (the
    /// crate's 250-nesting limit). Both are refused at frontend registration
    /// as [`RouterError::InvalidPathRule`], so they cost a loud rejection and
    /// not a mis-route — see `PathRule::anchored_regex`.
    ///
    /// To SEE THIS RED: delete the `Regex::new(value).ok()?;` validation line
    /// in `PathRule::anchored_regex`. The `a)(b` assertion then fails — the
    /// rule builds, and matches `ab`.
    #[test]
    fn anchoring_never_turns_a_rejected_path_regex_into_an_accepted_one() {
        // The hazard is real, not hypothetical. Both patterns are assembled
        // at run time rather than written as literals because
        // `clippy::invalid_regex` rejects an invalid literal at a `Regex::new`
        // call site — and the invalidity is the whole point here, so the
        // literal is what moves, not the lint.
        let unbalanced = format!("a{}b", ")(");
        assert!(
            Regex::new(&unbalanced).is_err(),
            "{unbalanced:?} must be an invalid regex for this test to mean anything",
        );
        let wrapped = Regex::new(&format!("\\A(?:{unbalanced})\\z")).expect(
            "the wrapped form of the unbalanced pattern compiles — that balancing \
             is what the pre-validation exists to defeat",
        );
        assert!(
            wrapped.is_match(b"ab"),
            "{} matches `ab`, which the operator never wrote",
            wrapped.as_str(),
        );

        for invalid in [unbalanced.as_str(), "(", ")", "[a-", "*"] {
            assert_eq!(
                PathRule::from_config(CommandPathRule::regex(invalid.to_owned())),
                None,
                "{invalid:?} must stay rejected once the value is anchored",
            );
        }
    }

    /// Anchoring narrows `Route::Deny` exactly as it narrows a forward, and
    /// that is the dangerous edge of the change: a deny rule written loosely
    /// stops covering the family of paths it used to cover. `/admin` denied
    /// `/admin`, `/admin/secret` and `/administrator`; anchored it denies
    /// `/admin` alone and the rest fall through to whatever else matches.
    ///
    /// Both rules carry a matching `method`, so the deny is tier 1 in
    /// `doc/configure.md`'s precedence list and ends the scan when it matches
    /// — the fall-through below is the deny genuinely not matching, not the
    /// prefix outranking it.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. Both fall-through assertions
    /// then fail with `left: None, right: Some("FALLBACK")`.
    #[test]
    fn anchoring_narrows_a_deny_path_regex_to_the_exact_path() {
        let mut router = Router::new();
        let deny = PathRule::from_config(CommandPathRule::regex("/admin".to_owned()))
            .expect("the deny regex path rule must build");
        assert!(router.add_tree_rule(
            b"www.example.com",
            &deny,
            &MethodRule::new(Some("GET".to_owned())),
            &Route::Deny,
        ));
        assert!(add_path_rule(
            &mut router,
            PathRule::Prefix("/".to_owned()),
            GET,
            "FALLBACK",
        ));

        let resolve = |path: &str| {
            router
                .lookup("www.example.com", path, &Method::Get)
                .expect("every path below is covered by the `/` prefix rule")
        };

        let denied = resolve("/admin");
        assert_eq!(
            denied.redirect,
            RedirectPolicy::Unauthorized,
            "the exact path the deny regex spells must still be denied",
        );
        assert_eq!(denied.cluster_id, None);

        for still_reaching_the_backend in ["/admin/secret", "/administrator"] {
            let result = resolve(still_reaching_the_backend);
            assert_eq!(
                result.cluster_id.as_deref(),
                Some("FALLBACK"),
                "BREAKING: the anchored deny no longer covers \
                 {still_reaching_the_backend}, which now reaches the backend",
            );
            assert_eq!(result.redirect, RedirectPolicy::Forward);
        }
    }

    /// `doc/configure.md`'s remediation table, rows one and two: a
    /// SUBSTRING-shaped or PREFIX-shaped pattern narrows to the exact path it
    /// spells, and a TRAILING `.*` is what gives it back — those two shapes
    /// are open at the END, so that is the side the wildcard goes on.
    ///
    /// `bc` against `/abcd` is sozu#1350's own example and is pinned whole in
    /// `a_path_regex_is_anchored_at_both_ends_and_must_match_the_whole_request_path`;
    /// it is repeated here as the table's first row, beside its remediation.
    /// The table answers the prefix-shaped row twice, with `.*` or with
    /// `path_type = "PREFIX"`; only the regex answer is a regex claim, so only
    /// that one is asserted.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. Every narrowing row below flips
    /// to a match; the first failure is `"bc"` against `"/abcd"`.
    #[test]
    fn a_substring_or_prefix_shaped_path_regex_narrows_to_the_path_it_spells() {
        assert_regex_routing(&[
            // What the two shapes lose.
            ("bc", "/abcd", false),
            ("/ab", "/abcd", false),
            ("/api", "/api/v1", false),
            // …and what they keep: the path they spell whole.
            ("/api", "/api", true),
            // The table says `bc` now matches "only the path `bc`"; that is
            // two claims, and this is the half the narrowing rows do not
            // cover.
            ("bc", "bc", true),
            // The documented remediation, on the side each pattern is open on.
            (".*bc.*", "/abcd", true),
            ("/api.*", "/api/v1", true),
            ("/api.*", "/api", true),
        ]);

        // The table's "used to match" column, measured. These go through the
        // BARE, unwrapped regex — the 2.2.1 code — so they are controls: no
        // mutation of `anchored_regex` can move them, and that is exactly
        // what makes them evidence for what the release took away. Without
        // them the whole first column is prose.
        for (pattern, used_to_match) in [("bc", "/abcd"), ("/api", "/api/v1")] {
            assert!(
                Regex::new(pattern)
                    .expect("the bare pattern compiles")
                    .is_match(used_to_match.as_bytes()),
                "up to 2.2.1 the unwrapped {pattern:?} matched {used_to_match:?} \
                 as a substring — that is the behaviour this release removes",
            );
        }
    }

    /// **The row that breaks silently.** A SUFFIX-shaped pattern is open at
    /// the START, so it needs a LEADING `.*`; the trailing one that answers
    /// the substring and prefix shapes does nothing for it. Extension routing
    /// is plausibly the commonest real use of a path regex, and it does not
    /// narrow — it stops matching.
    ///
    /// Every verdict below is `doc/configure.md`'s third table row and the two
    /// paragraphs under it, measured.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. `"\.js$"` against
    /// `"/static/app.js"` fails first, and every other `false` row above the
    /// leading-`.*` ones fails with it.
    #[test]
    fn a_suffix_shaped_path_regex_needs_a_leading_wildcard_not_a_trailing_one() {
        assert_regex_routing(&[
            // The three the documentation names, all silently dead.
            (r"\.js$", "/static/app.js", false),
            (r"\.(js|css)$", "/a/b/style.css", false),
            ("/v1$", "/api/v1", false),
            // `/v1$` is the less dramatic half: it still matches its own path.
            ("/v1$", "/v1", true),
            // The trailing `.*` that rescues the other two shapes does NOT
            // rescue this one.
            (r"\.js.*", "/static/app.js", false),
            ("/v1.*", "/api/v1", false),
            // The leading one does.
            (r".*\.js$", "/static/app.js", true),
            (r".*\.(js|css)$", "/a/b/style.css", true),
            (".*/v1$", "/api/v1", true),
        ]);

        // "…matches nothing a client can send": an end-anchored pattern with
        // nothing in front of it can only match a path that IS its own
        // suffix, which for `\.js$` is the literal path `.js`. An origin-form
        // request-target always begins with `/`, so no client can send it.
        assert_regex_routing(&[(r"\.js$", ".js", true)]);

        // The table's third "used to match" cell, measured against the BARE
        // regex of 2.2.1 — a control, like the two in
        // `a_substring_or_prefix_shaped_path_regex_narrows_to_the_path_it_spells`.
        // This is the cell that matters most: it is the one whose loss is
        // silent.
        assert!(
            Regex::new(r"\.js$")
                .expect("the bare suffix pattern compiles")
                .is_match(b"/static/app.js"),
            "up to 2.2.1 the unwrapped `\\.js$` DID match `/static/app.js` — \
             losing that is the silent break this release documents",
        );
        for origin_form in ["/", "/.js", "/app.js", "/static/app.js"] {
            assert!(
                regex_rule_routes(r"\.js$", origin_form).is_none(),
                "an origin-form request-target begins with `/`, and {origin_form} \
                 must not match the end-anchored `\\.js$`",
            );
        }
    }

    /// **The `$PATH[n]` regression test.** The anchoring group is `(?:` … `)`
    /// and not `(` … `)` precisely so it adds no capture. `Router::lookup`
    /// collects path captures as `caps.iter().skip(1)` and `Frontend::new`
    /// sizes the buffer from `PathRule::Regex(regex) => regex.captures_len()`,
    /// so a CAPTURING wrapper would bump that cap and shift every configured
    /// `$PATH[n]` by one: a frontend asking for its first group would start
    /// receiving the whole request path, and keep serving traffic while doing
    /// it. This is the one property here that corrupts silently rather than
    /// stops matching.
    ///
    /// Asserted three ways: `captures_len()` parity against the bare pattern
    /// for zero, one and two groups; end to end, that `rewrite_path` of
    /// `$PATH[1]` still receives the first group's text; and that the CAP
    /// itself did not grow, by pinning that a `$PATH[n]` one past the last
    /// group the operator wrote is still refused.
    ///
    /// To SEE THIS RED: make the wrapper capturing in
    /// `PathRule::anchored_regex` — `format!("\\A({value})\\z")`. The parity
    /// assertion fails first with `left: 2, right: 1` on the group-less
    /// pattern; with that loop deleted the rewrite yields `/api/v2/users`
    /// instead of `v2`, and the out-of-range `$PATH[2]` registers instead of
    /// being refused.
    #[test]
    fn the_anchoring_group_is_non_capturing_so_path_rewrite_indices_do_not_shift() {
        for pattern in ["/api/v2/.*", "/api/(v[0-9]+)/.*", "/api/(v[0-9]+)/(.*)"] {
            let bare = Regex::new(pattern).expect("the bare pattern must compile");
            let anchored =
                PathRule::anchored_regex(pattern).expect("the anchored pattern must compile");
            assert_eq!(
                anchored.captures_len(),
                bare.captures_len(),
                "anchoring {pattern:?} must not add a capture group",
            );
        }

        let rewriting_frontend = |pattern: &str, rewrite: &str| {
            let mut front = test_http_frontend();
            front.hostname = "www.example.com".to_owned();
            front.path = CommandPathRule::regex(pattern.to_owned());
            front.rewrite_path = Some(rewrite.to_owned());
            front
        };
        let rewritten = |pattern: &str, rewrite: &str, path: &str| {
            let mut router = Router::new();
            router
                .add_http_front(&rewriting_frontend(pattern, rewrite))
                .expect("the rewriting frontend must register");
            router
                .lookup("www.example.com", path, &Method::Get)
                .expect("the request must route")
                .rewritten_path
        };

        assert_eq!(
            rewritten("/api/(v[0-9]+)/.*", "$PATH[1]", "/api/v2/users").as_deref(),
            Some("v2"),
            "`$PATH[1]` must still be the FIRST group the operator wrote, not \
             the anchoring wrapper",
        );
        assert_eq!(
            rewritten("/api/(v[0-9]+)/(.*)", "/$PATH[2]", "/api/v2/users").as_deref(),
            Some("/users"),
            "`$PATH[2]` must still be the SECOND group the operator wrote",
        );
        assert_eq!(
            rewritten("/api/(v[0-9]+)/.*", "$PATH[0]", "/api/v2/users").as_deref(),
            Some("/api/v2/users"),
            "`$PATH[0]` stays the whole request path",
        );

        let mut router = Router::new();
        assert!(
            matches!(
                router.add_http_front(&rewriting_frontend("/api/(v[0-9]+)/.*", "$PATH[2]")),
                Err(RouterError::InvalidPathRewrite(_)),
            ),
            "the capture CAP is `captures_len()` too: a `$PATH[n]` past the \
             last group the operator wrote must stay refused",
        );
    }

    /// The pre-validation in `PathRule::anchored_regex` closes
    /// invalid-becomes-valid only. The opposite direction is real, documented
    /// and NOT closed: a pattern that compiles BARE can be refused once
    /// wrapped, because the appended `)\z` has to survive whatever the pattern
    /// left open.
    ///
    /// Both halves are asserted for each case — `Regex::new(value).is_ok()`
    /// AND `from_config(…).is_none()` — because the second alone would not
    /// show the wrapping is what rejected it.
    ///
    /// The `249` both `CHANGELOG.md` and `doc/configure.md` quote is measured
    /// here rather than derived from the crate's documented limit, and it is
    /// the SMALLEST depth with this property: on `regex` 1.13.1 depth 248
    /// still survives the wrapping, depth 249 does not, and depth 251 no
    /// longer compiles bare either.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. Both `from_config` assertions
    /// fail, each rule building instead of being refused. (The `Regex::new`
    /// rows are controls measuring the crate directly and do not move.)
    #[test]
    fn a_path_regex_that_compiles_bare_can_be_refused_once_it_is_anchored() {
        // `(?x)` turns on extended mode, in which `#` starts a comment that
        // runs to end of line — and the wrapper's `)` lands inside it.
        let extended_mode_comment = "(?x)/api/v1 # v1 only".to_owned();
        let nested = |depth: usize| format!("{}{}", "(".repeat(depth), ")".repeat(depth));

        for (value, expected_error) in [
            (extended_mode_comment, "unclosed group"),
            (
                nested(249),
                "exceed the maximum number of nested parentheses/brackets (250)",
            ),
        ] {
            assert!(
                Regex::new(&value).is_ok(),
                "this case only means something while the BARE pattern compiles",
            );
            let error = Regex::new(&format!("\\A(?:{value})\\z"))
                .expect_err("the WRAPPED pattern is the one that must fail");
            assert!(
                error.to_string().contains(expected_error),
                "the documentation quotes {expected_error:?}; the wrapping \
                 actually failed with: {error}",
            );
            assert_eq!(
                PathRule::from_config(CommandPathRule::regex(value)),
                None,
                "a value only the WRAPPING rejects must still be refused",
            );
        }

        // The boundary itself, so the quoted 249 cannot drift unnoticed.
        assert!(
            Regex::new(&format!("\\A(?:{})\\z", nested(248))).is_ok(),
            "depth 248 must still survive the wrapping, or 249 is not the boundary",
        );
        assert!(
            Regex::new(&nested(250)).is_ok() && Regex::new(&nested(251)).is_err(),
            "the BARE limit must still sit between 250 and 251, or the wrapper \
             is no longer what makes depth 249 fail",
        );
    }

    /// Every pattern `PathRule::from_config` refuses — bare-invalid or
    /// invalid-only-once-wrapped — surfaces at the public registration
    /// surface as [`RouterError::InvalidPathRule`]. That is the same
    /// `Router::add_http_front` the main process runs through
    /// `validate_frontend_request` (`bin/src/command/requests.rs`) before
    /// fanning a frontend out, which is what makes such a pattern a loudly
    /// rejected frontend and never a main/worker divergence.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. The last two rows then
    /// register successfully. To redden the FIRST row instead, delete the
    /// `Regex::new(value).ok()?;` validation line: the wrapper balances
    /// `a)(b` and the frontend registers.
    #[test]
    fn a_refused_path_regex_surfaces_as_an_invalid_path_rule_at_registration() {
        // Assembled rather than written as a literal: `clippy::invalid_regex`
        // rejects an invalid literal at a `Regex::new` call site, and the
        // invalidity is the point.
        let unbalanced = format!("a{}b", ")(");
        for value in [
            unbalanced,
            "(?x)/api/v1 # v1 only".to_owned(),
            format!("{}{}", "(".repeat(249), ")".repeat(249)),
        ] {
            let mut router = Router::new();
            let mut front = test_http_frontend();
            front.path = CommandPathRule::regex(value.clone());
            assert!(
                matches!(
                    router.add_http_front(&front),
                    Err(RouterError::InvalidPathRule(_))
                ),
                "the path regex {value:?} must be refused as an invalid path rule",
            );
        }
    }

    /// The FIRST of the two 401 shapes `doc/configure.md` names: a frontend
    /// with no `cluster_id`. It narrows exactly as the
    /// `redirect = "unauthorized"` shape does, which is the substance of
    /// "both answer 401 … through the very same `path`/`path_type` matching".
    ///
    /// Asserted through `Router::add_http_front`, because that is where the
    /// shape becomes a route and the claim is about the FRONTEND, not about
    /// `Route::Deny`. The two sub-shapes take different branches and both are
    /// covered here: a bare clusterless frontend sets no policy field, so
    /// `add_http_front`'s `has_policy` is false and it maps to `Route::Deny`
    /// directly; one that spells `redirect = "forward"` flips `has_policy`,
    /// runs `Frontend::new`, and is coerced to unauthorized there instead.
    /// Nothing in the anchoring cares which — that is the point.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. The unanchored `/admin` covers
    /// the family again and both fall-through assertions fail with
    /// `left: None, right: Some("FALLBACK")`.
    #[test]
    fn a_clusterless_regex_frontend_denies_through_the_same_anchored_path_rule() {
        for spells_forward in [false, true] {
            let mut router = Router::new();
            let mut clusterless = test_http_frontend();
            clusterless.hostname = "www.example.com".to_owned();
            clusterless.cluster_id = None;
            clusterless.path = CommandPathRule::regex("/admin".to_owned());
            clusterless.method = Some("GET".to_owned());
            if spells_forward {
                clusterless.redirect = Some(RedirectPolicy::Forward as i32);
            }
            router
                .add_http_front(&clusterless)
                .expect("a clusterless frontend must register as a deny");
            assert!(add_path_rule(
                &mut router,
                PathRule::Prefix("/".to_owned()),
                GET,
                "FALLBACK",
            ));

            let resolve = |path: &str| {
                router
                    .lookup("www.example.com", path, &Method::Get)
                    .expect("every path below is covered by the `/` prefix rule")
            };

            let denied = resolve("/admin");
            assert_eq!(
                denied.redirect,
                RedirectPolicy::Unauthorized,
                "a clusterless frontend must still deny the path it spells \
                 (spells_forward={spells_forward})",
            );
            assert_eq!(denied.cluster_id, None);

            for now_reaching_the_backend in ["/admin/secret", "/administrator"] {
                assert_eq!(
                    resolve(now_reaching_the_backend).cluster_id.as_deref(),
                    Some("FALLBACK"),
                    "BREAKING: the anchored clusterless deny no longer covers \
                     {now_reaching_the_backend} (spells_forward={spells_forward})",
                );
            }
        }
    }

    /// The documented remediation for the deny narrowing: a deny-shaped
    /// `REGEX` meant as a subtree is widened with a trailing `.*` and covers
    /// the family again. It is a regex and not a path-segment rule, so
    /// `/admin.*` takes `/administrator` back with `/admin/secret` — widening
    /// is not surgical, and that is worth seeing.
    ///
    /// Declared through `Router::add_http_front` carrying
    /// `redirect = "unauthorized"`, which is the SECOND of the two 401 shapes
    /// `doc/configure.md` names. The first — a frontend with no `cluster_id` —
    /// is covered by
    /// `a_clusterless_regex_frontend_denies_through_the_same_anchored_path_rule`.
    /// `anchoring_narrows_a_deny_path_regex_to_the_exact_path` covers NEITHER
    /// frontend shape: it builds `Route::Deny` directly through
    /// `add_tree_rule`, which is the route those two shapes resolve TO, and
    /// says nothing about either resolving to it.
    ///
    /// To SEE THIS RED: the anchoring mutation does NOT redden this test, and
    /// cannot — `/admin.*` covers these paths anchored or not, which is
    /// exactly why it is the remediation. Break the wildcard instead: take the
    /// configured value literally in `PathRule::anchored_regex`, as
    /// `Regex::new(&format!("\\A(?:{})\\z", regex::escape(value)))`. Every
    /// denied path below then falls through to `FALLBACK`.
    #[test]
    fn widening_a_deny_path_regex_with_a_trailing_wildcard_covers_the_subtree_again() {
        let mut router = Router::new();
        let mut deny = test_http_frontend();
        deny.hostname = "www.example.com".to_owned();
        deny.cluster_id = Some("DENIED".to_owned());
        deny.path = CommandPathRule::regex("/admin.*".to_owned());
        deny.method = Some("GET".to_owned());
        deny.redirect = Some(RedirectPolicy::Unauthorized as i32);
        router
            .add_http_front(&deny)
            .expect("the widened deny frontend must register");
        assert!(add_path_rule(
            &mut router,
            PathRule::Prefix("/".to_owned()),
            GET,
            "FALLBACK",
        ));

        let resolve = |path: &str| {
            router
                .lookup("www.example.com", path, &Method::Get)
                .expect("every path below is covered by the `/` prefix rule")
        };

        for denied in ["/admin", "/admin/secret", "/administrator"] {
            assert_eq!(
                resolve(denied).redirect,
                RedirectPolicy::Unauthorized,
                "the widened deny must cover {denied} again",
            );
        }
        assert_eq!(
            resolve("/other").cluster_id.as_deref(),
            Some("FALLBACK"),
            "widening the deny must not turn it into a catch-all",
        );
        assert_eq!(
            resolve("/other").redirect,
            RedirectPolicy::Forward,
            "a path outside the widened family must still be forwarded",
        );
    }

    /// The matched request path is the request-target as it arrives, QUERY
    /// STRING INCLUDED, so anchoring bites a query string the way it already
    /// bit `path_type = "EQUALS"`. A trailing `.*` answers that only for a
    /// pattern open at the END: after a `$` it is dead text, because nothing
    /// can follow end-of-text. A suffix rule that must tolerate a query string
    /// has to spell it out.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. `"/abc"` against `"/abc?x=1"`
    /// fails first, and `".*\.js([?].*)?"` against `"/static/app.json"` fails
    /// with it, the optional group matching empty after a substring `.js`.
    #[test]
    fn an_anchored_path_regex_must_span_the_query_string_it_is_matched_against() {
        assert_regex_routing(&[
            ("/abc", "/abc", true),
            ("/abc", "/abc?x=1", false),
            // Appending `.*` after a `$` does nothing at all.
            ("^/abc$.*", "/abc", true),
            ("^/abc$.*", "/abc?x=1", false),
            // What a suffix rule surviving a query string has to look like.
            (r".*\.js([?].*)?", "/static/app.js", true),
            (r".*\.js([?].*)?", "/static/app.js?v=2", true),
            (r".*\.js([?].*)?", "/static/app.json", false),
        ]);

        // "not a regression": the plain suffix rule did not match a
        // query-string request before the anchoring either. Measured against
        // the BARE, unwrapped regex — the 2.2.1 behaviour — so this row is a
        // control and does not move when `anchored_regex` does.
        assert!(
            !Regex::new(r"\.js$")
                .expect("the bare suffix pattern compiles")
                .is_match(b"/static/app.js?v=2"),
            "the unanchored `\\.js$` of 2.2.1 did not match a query-string \
             request either, so that half is not a regression",
        );
    }

    /// Precedence between a method-less `REGEX` and a method-less `PREFIX`
    /// turns on whether the regex matches the request path AT ALL, and
    /// anchoring changes that once a query string is involved.
    ///
    /// `doc/configure.md`'s third table row is the `/a.*` case: `GET /abc`
    /// goes to the prefix, and `GET /abc?x=1` flips to the regex, because
    /// `/abc` no longer spans the request path while `/a.*` still does. With
    /// the regex `/abc`, which anchored does not match `/abc?x=1` at all, the
    /// flip does NOT happen and both requests go to the prefix — the prefix
    /// has merely lost its rival. The regex did not lose the contest, it
    /// stopped entering it: declared alone it leaves `GET /abc?x=1` unrouted.
    /// That distinction is stated in prose and is what these six cases pin.
    ///
    /// Both rules are method-less and the regex is declared FIRST, the exact
    /// shape the documented table measures; the regex is built through
    /// `PathRule::from_config`, so the anchoring is in play.
    ///
    /// To SEE THIS RED: remove the anchoring in `PathRule::anchored_regex` —
    /// make the body `Regex::new(value).ok()`. The unanchored `/abc` matches
    /// `/abc?x=1` as a substring and becomes a candidate again: the
    /// both-go-to-the-prefix case fails with
    /// `left: Some("REGEX"), right: Some("PREFIX")` and the declared-alone
    /// case fails with `left: Some("REGEX"), right: None`.
    #[test]
    fn an_anchored_regex_that_cannot_span_the_query_string_stops_being_a_candidate() {
        let configured = |pattern: &str| {
            PathRule::from_config(CommandPathRule::regex(pattern.to_owned()))
                .unwrap_or_else(|| panic!("the regex path rule {pattern:?} must build"))
        };
        let against_the_whole_path_prefix = |pattern: &str, path: &str| {
            winner_of(
                (configured(pattern), NO_METHOD, "REGEX"),
                (PathRule::Prefix("/abc".to_owned()), NO_METHOD, "PREFIX"),
                path,
            )
        };

        // A regex that spans the query string: the documented flip, unchanged.
        assert_eq!(
            against_the_whole_path_prefix("/a.*", "/abc").as_deref(),
            Some("PREFIX"),
            "the whole-path prefix declared after the regex must still win",
        );
        assert_eq!(
            against_the_whole_path_prefix("/a.*", "/abc?x=1").as_deref(),
            Some("REGEX"),
            "with a query string `/abc` no longer spans the request path, so \
             a regex that does wins",
        );

        // A regex that does not span it: the flip disappears entirely.
        assert_eq!(
            against_the_whole_path_prefix("/abc", "/abc").as_deref(),
            Some("PREFIX"),
            "the prefix wins here exactly as it does against `/a.*`",
        );
        assert_eq!(
            against_the_whole_path_prefix("/abc", "/abc?x=1").as_deref(),
            Some("PREFIX"),
            "an anchored `/abc` cannot match `/abc?x=1`, so adding a query \
             string changes nothing and the prefix keeps the request",
        );

        // …and the reason is candidacy, not defeat.
        assert_eq!(
            regex_rule_routes("/abc", "/abc").as_deref(),
            Some("REGEX"),
            "declared alone, the regex answers the request it spells",
        );
        assert_eq!(
            regex_rule_routes("/abc", "/abc?x=1"),
            None,
            "declared alone, it leaves the query-string request UNROUTED — it \
             is not a candidate any more",
        );
    }

    /// Every branch of an alternation in a regex hostname SEGMENT is
    /// anchored, not just the first and the last.
    ///
    /// This test is the INVERSION of the test formerly named
    /// `an_alternating_regex_hostname_segment_is_still_anchored_at_one_end_only`
    /// — a name no longer in the tree, this one having replaced it —
    /// which pinned the defect so that closing it would be "a deliberate
    /// change, not a silent one" and asked, in its own comment, to be
    /// inverted rather than deleted when the fix landed. This is that
    /// deliberate change (sozu#1356), and the old name is kept here so the
    /// behaviour change leaves a trace.
    ///
    /// `pattern_trie.rs`'s `anchored_segment` now builds
    /// `\A(?:{s})\z`. Ungrouped, `|` binds looser than concatenation, so
    /// `\Aa|b\z` parsed as `(\Aa)|(b\z)`: a label merely STARTING with `a`
    /// or merely ENDING with `b` reached the frontend.
    ///
    /// Two branches cannot show what this test claims, because with two
    /// branches "first and last" is every branch there is. The three-branch
    /// case below has a MIDDLE branch, which ungrouped keeps NEITHER anchor
    /// and matches as a bare substring anywhere in the label — strictly
    /// worse than the half-anchored ends.
    ///
    /// To SEE THIS RED: drop the group in `pattern_trie.rs`'s
    /// `anchored_segment` — build `format!("\\A{segment}\\z")`, the form
    /// this tree carried up to 2.2.1, and relax the `debug_assert!` beside it,
    /// which in a debug build fires first. `a.example.com` still resolves,
    /// and the `axx.example.com` assertion fails with
    /// `left: Some("ALTERNATION"), right: None`.
    #[test]
    fn an_alternating_regex_hostname_segment_is_anchored_on_every_branch() {
        let resolve_with = |pattern: &[u8], hostname: &str| {
            let mut router = Router::new();
            assert!(router.add_tree_rule(
                pattern,
                &PathRule::Prefix("/".to_owned()),
                &MethodRule::new(Some("GET".to_owned())),
                &Route::ClusterId("ALTERNATION".to_owned()),
            ));
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        // Two branches: each one keeps BOTH anchors now.
        assert_eq!(
            resolve_with(b"/a|b/.example.com", "a.example.com").as_deref(),
            Some("ALTERNATION"),
            "the segment must still match what it was written for",
        );
        assert_eq!(
            resolve_with(b"/a|b/.example.com", "b.example.com").as_deref(),
            Some("ALTERNATION"),
            "and the other branch too",
        );
        assert_eq!(
            resolve_with(b"/a|b/.example.com", "axx.example.com"),
            None,
            "the first branch must be anchored at its END too",
        );
        assert_eq!(
            resolve_with(b"/a|b/.example.com", "xxb.example.com"),
            None,
            "the last branch must be anchored at its START too",
        );
        assert_eq!(
            resolve_with(b"/a|b/.example.com", "xx.example.com"),
            None,
            "a label matching neither branch must still miss",
        );

        // Three branches. `zzbzz` is the case two branches cannot express:
        // ungrouped, the MIDDLE branch carries no anchor at either end, so
        // the bare label `b` matches in the middle of any label at all.
        for whole in ["a.example.com", "b.example.com", "c.example.com"] {
            assert_eq!(
                resolve_with(b"/a|b|c/.example.com", whole).as_deref(),
                Some("ALTERNATION"),
                "{whole} is a whole-label match for one branch and must route",
            );
        }
        for leak in [
            "axx.example.com",
            "xxc.example.com",
            "zzbzz.example.com",
            "bzz.example.com",
            "zzb.example.com",
        ] {
            assert_eq!(
                resolve_with(b"/a|b|c/.example.com", leak),
                None,
                "{leak} matches no branch WHOLE and must not route",
            );
        }

        // The control for all of the above, built by hand against the
        // `regex` crate rather than through the trie: the UNGROUPED wrapper
        // — the form this tree carried up to 2.2.1 — leaves the middle
        // branch anchored at neither end. No mutation of `anchored_segment`
        // can move this assertion, which is the point: it measures why the
        // group is there.
        let two = Regex::new("\\Aa|b\\z").expect("the ungrouped wrapper compiles");
        for anywhere in ["axx", "xxb"] {
            assert!(
                two.is_match(anywhere.as_bytes()),
                "{} leaks on {anywhere}",
                two.as_str(),
            );
        }
        let ungrouped = Regex::new("\\Aa|b|c\\z").expect("the ungrouped wrapper compiles");
        for anywhere in ["axx", "zzbzz", "bzz", "zzb", "xxc"] {
            assert!(
                ungrouped.is_match(anywhere.as_bytes()),
                "{} leaks on {anywhere}",
                ungrouped.as_str(),
            );
        }
        let grouped = Regex::new("\\A(?:a|b|c)\\z").expect("the grouped wrapper compiles");
        for anywhere in ["axx", "zzbzz", "bzz", "zzb", "xxc"] {
            assert!(
                !grouped.is_match(anywhere.as_bytes()),
                "{} must refuse {anywhere}",
                grouped.as_str(),
            );
        }
    }

    /// Declare one regex hostname frontend the way a configured frontend
    /// declares it — through `Router::add_http_front`, so the hostname goes
    /// through `DomainRule::from_str` and `convert_regex_domain_rule` — and
    /// resolve `hostname`.
    ///
    /// `position` picks the code path, and the two are genuinely different
    /// code: `Pre`/`Post` match the WHOLE host against the single regex
    /// `convert_regex_domain_rule` assembles, while `Tree` matches label by
    /// label against `pattern_trie`'s per-segment regexes.
    fn regex_host_routes(position: RulePosition, pattern: &str, hostname: &str) -> Option<String> {
        let mut front = test_http_frontend();
        front.hostname = pattern.to_owned();
        front.position = position;
        front.cluster_id = Some("REGEX-HOST".to_owned());
        let mut router = Router::new();
        router
            .add_http_front(&front)
            .unwrap_or_else(|error| panic!("the regex hostname {pattern:?} must build: {error}"));
        router
            .lookup(hostname, "/", &Method::Get)
            .ok()
            .and_then(|result| result.cluster_id)
    }

    /// The companion of
    /// `an_alternating_regex_hostname_segment_is_anchored_on_every_branch`
    /// for the OTHER hostname code path: a pre/post rule never touches the
    /// trie, it matches the whole host against the single regex
    /// `convert_regex_domain_rule` assembles, and that assembly carried the
    /// same ungrouped `\A` … `\z` (sozu#1356).
    ///
    /// The exact assembled pattern is asserted, so "grouped" is pinned and
    /// not merely implied by the routing verdicts.
    ///
    /// Two branches cannot show what this test claims — with two branches
    /// "first and last" is every branch. The three-branch case has a MIDDLE
    /// branch, which ungrouped keeps NEITHER anchor: it is a bare `b`,
    /// matching any hostname on earth that contains a `b`, including one on
    /// a different registrable domain entirely.
    ///
    /// To SEE THIS RED: drop the group in `convert_regex_domain_rule` —
    /// replace the `push_str`/`push` calls in the `/` arm with the single
    /// `result.push_str(r)` this tree carried up to 2.2.1. The
    /// `axx.example.com` row then fails with
    /// `left: Some("REGEX-HOST"), right: None`, and the exact-pattern
    /// assertion below it with
    /// `left: "\\Aa|b|c\\.example\\.com\\z", right: "\\A(?:a|b|c)\\.example\\.com\\z"`.
    #[test]
    fn every_branch_of_an_alternating_regex_hostname_rule_is_anchored() {
        for position in [RulePosition::Pre, RulePosition::Post] {
            for whole in ["a.example.com", "b.example.com", "c.example.com"] {
                assert_eq!(
                    regex_host_routes(position, "/a|b|c/.example.com", whole).as_deref(),
                    Some("REGEX-HOST"),
                    "{position:?}: {whole} is a whole-host match for one branch and must route",
                );
            }
            for leak in [
                "axx.example.com",
                "xxc.example.com",
                "zzbzz.example.com",
                "b.example.com.evil.org",
                "zzbzz.evil.org",
            ] {
                assert_eq!(
                    regex_host_routes(position, "/a|b|c/.example.com", leak),
                    None,
                    "{position:?}: {leak} matches no branch WHOLE and must not route",
                );
            }
        }

        // The exact assembled pattern, so "grouped" is asserted and not
        // merely implied by the routing verdicts above.
        assert_eq!(
            convert_regex_domain_rule("/a|b|c/.example.com")
                .expect("an alternating hostname segment must convert"),
            "\\A(?:a|b|c)\\.example\\.com\\z",
            "each regex segment must be wrapped in a NON-capturing group",
        );

        // The control, built by hand against the `regex` crate rather than
        // through `convert_regex_domain_rule`: the UNGROUPED assembly — the
        // form this tree carried up to 2.2.1 — parses as
        // `(\Aa)|(b)|(c\.example\.com\z)`. The middle branch is a bare `b`,
        // so it matches a host on an unrelated registrable domain. No
        // mutation of `convert_regex_domain_rule` can move these
        // assertions, which is the point: they measure why the group is
        // there.
        for (assembly, leaks) in [
            (
                "\\Aa|b|c\\.example\\.com\\z",
                &[
                    "axx.example.com",
                    "zzbzz.example.com",
                    "b.example.com.evil.org",
                    "zzbzz.evil.org",
                ][..],
            ),
            (
                "\\Aapi|admin\\.example\\.com\\z",
                &["apifoo.example.com", "xadmin.example.com"][..],
            ),
        ] {
            let ungrouped = Regex::new(assembly).expect("the ungrouped assembly compiles");
            for anywhere in leaks {
                assert!(
                    ungrouped.is_match(anywhere.as_bytes()),
                    "{} leaks on {anywhere}",
                    ungrouped.as_str(),
                );
            }
        }

        // `doc/configure.md`'s own worked example, asserted here because it
        // is written there as a measurement: the frontend
        // `/api|admin/.example.com` used to also serve `apifoo.example.com`
        // and `xadmin.example.com`, and the remediation the same paragraph
        // gives has to keep working.
        for (pattern, hostname, must_route) in [
            ("/api|admin/.example.com", "api.example.com", true),
            ("/api|admin/.example.com", "admin.example.com", true),
            ("/api|admin/.example.com", "apifoo.example.com", false),
            ("/api|admin/.example.com", "xadmin.example.com", false),
            ("/api.*|.*admin/.example.com", "api.example.com", true),
            ("/api.*|.*admin/.example.com", "admin.example.com", true),
            ("/api.*|.*admin/.example.com", "apifoo.example.com", true),
            ("/api.*|.*admin/.example.com", "xadmin.example.com", true),
        ] {
            assert_eq!(
                regex_host_routes(RulePosition::Pre, pattern, hostname).is_some(),
                must_route,
                "hostname regex {pattern:?} against {hostname:?}",
            );
        }

        // `captures_len` must be untouched by the wrapping — see
        // `a_host_capture_rewrite_resolves_to_the_operators_own_group` for
        // the consequence when it is not.
        let compiled = |pattern: &str| {
            Regex::new(&convert_regex_domain_rule(pattern).expect("the hostname must convert"))
                .expect("the assembled hostname regex must compile")
        };
        assert_eq!(
            compiled("/a|b|c/.example.com").captures_len(),
            1,
            "a pattern with no operator group must stay at the implicit group 0 alone",
        );
        assert_eq!(
            compiled("/cdn([0-9]+)/.example.com").captures_len(),
            2,
            "the operator's own group must stay at index 1",
        );
    }

    /// A `*` label in a hostname that ALSO carries a regex segment means two
    /// different things depending on the rule position, and
    /// `doc/configure.md` § "Regex hostname segments" now carries the table
    /// this test asserts.
    ///
    /// On the trie a `*` label is the ordinary single-label wildcard: the
    /// trie splits on `.` and `TrieNode` stores it in its `wildcard` slot,
    /// untouched by anything here. On `Pre`/`Post` the whole host is matched
    /// by the single pattern `convert_regex_domain_rule` assembles, where the
    /// `*` is now a LITERAL — the sozu#1356 escaping. Up to and including
    /// 2.2.1 it was neither: an unescaped `*` was a regex quantifier over the
    /// preceding atom, which for a leftmost label is the opening `\A`, so the
    /// pattern was not anchored at its start and took any host ending in
    /// `.x.example.com` at any depth.
    ///
    /// The remediation the section gives for `Pre`/`Post` is asserted too,
    /// because a documented remediation that does not work is worse than
    /// none.
    ///
    /// To SEE THIS RED: drop the escaping in the literal arm of
    /// `convert_regex_domain_rule` — push `r` instead of `&regex::escape(r)`
    /// at both exits. The `Pre` row for `zz.x.example.com` then routes and
    /// fails with `left: true, right: false`. (That mutation also reddens
    /// `a_literal_hostname_label_is_escaped_and_cannot_become_a_pattern` and
    /// `convert_regex`.)
    #[test]
    fn a_star_label_is_a_wildcard_on_the_trie_and_a_literal_on_pre_and_post() {
        // The table in `doc/configure.md`, both rows, all three positions.
        for (position, host, must_route) in [
            (RulePosition::Tree, "zz.x.example.com", true),
            (RulePosition::Tree, "*.x.example.com", true),
            (RulePosition::Pre, "zz.x.example.com", false),
            (RulePosition::Pre, "*.x.example.com", true),
            (RulePosition::Post, "zz.x.example.com", false),
            (RulePosition::Post, "*.x.example.com", true),
        ] {
            assert_eq!(
                regex_host_routes(position, "*./x/.example.com", host).is_some(),
                must_route,
                "{position:?}: `*./x/.example.com` against {host:?}",
            );
        }

        // Common to every position, and the reason the trie row is a
        // wildcard and not a free-for-all: `*` is ONE label.
        for position in [RulePosition::Tree, RulePosition::Pre, RulePosition::Post] {
            for miss in ["x.example.com", "a.b.x.example.com"] {
                assert_eq!(
                    regex_host_routes(position, "*./x/.example.com", miss),
                    None,
                    "{position:?}: a `*` label is exactly one label, so {miss:?} misses",
                );
            }
        }

        // What 2.2.1 did on `Pre`/`Post`, built by hand against the crate:
        // `\A*` repeats a zero-width assertion, so the pattern was not
        // anchored at its start. No mutation of the escaping can move this.
        let quantified =
            Regex::new("\\A*\\.(?:x)\\.example\\.com\\z").expect("the raw assembly compiles");
        for anywhere in [
            "evil.attacker.x.example.com",
            "anything.at.all.x.example.com",
            "zzz.x.example.com",
        ] {
            assert!(
                quantified.is_match(anywhere.as_bytes()),
                "{} took {anywhere} at any depth",
                quantified.as_str(),
            );
        }
        assert!(
            !quantified.is_match(b"x.example.com"),
            "{} still required a leading label",
            quantified.as_str(),
        );

        // The remediation `doc/configure.md` gives for `Pre`/`Post`: a regex
        // segment spelling the single label out.
        for position in [RulePosition::Pre, RulePosition::Post] {
            for (host, must_route) in [
                ("zz.x.example.com", true),
                ("*.x.example.com", true),
                ("a.b.x.example.com", false),
                ("x.example.com", false),
            ] {
                assert_eq!(
                    regex_host_routes(position, "/[^.]*/./x/.example.com", host).is_some(),
                    must_route,
                    "{position:?}: the documented `Pre`/`Post` remediation against {host:?}",
                );
            }
        }
    }

    /// A `.*` inside a regex segment does not stop at the label boundary, and
    /// where it stops depends on the rule POSITION — the two hostname paths
    /// genuinely differ here. A trie rule matches each segment against one
    /// label, so `.*` cannot leave it. A `Pre`/`Post` rule is matched by the
    /// single whole-host pattern `convert_regex_domain_rule` assembles, in
    /// which `.` matches the `.` separator like any other character.
    ///
    /// Pre-existing and NOT introduced by the anchoring, but
    /// `doc/configure.md` walks operators into it: the remediation it gives
    /// for the narrowing is exactly a `.*`. The section now names the
    /// divergence and recommends `[^.]*` on `Pre`/`Post`, so all three
    /// measurements it makes are pinned here.
    ///
    /// To SEE THIS RED: escape the REGEX-segment arm of
    /// `convert_regex_domain_rule` as well as the literal arm — push
    /// `&regex::escape(r)` in the `/` arm too. Every `Pre` row then goes
    /// false, starting with `api.example.com`, because the segment stops
    /// being a pattern at all.
    #[test]
    fn a_wildcard_in_a_hostname_segment_crosses_labels_on_pre_and_post_only() {
        // Trie: one segment, one label. `.*` cannot escape the label.
        for (hostname, must_route) in [
            ("api.example.com", true),
            ("admin.example.com", true),
            ("apifoo.example.com", true),
            ("api.foo.example.com", false),
            ("zz.admin.example.com", false),
        ] {
            assert_eq!(
                regex_host_routes(RulePosition::Tree, "/api.*|.*admin/.example.com", hostname)
                    .is_some(),
                must_route,
                "trie rule `/api.*|.*admin/` against {hostname:?}",
            );
        }

        // Pre/Post: one whole-host pattern, in which `.` matches the
        // separator. The same rule reaches subdomains the operator did not
        // write. This is the clause `doc/configure.md` now carries.
        for position in [RulePosition::Pre, RulePosition::Post] {
            for hostname in [
                "api.example.com",
                "admin.example.com",
                "apifoo.example.com",
                "api.foo.example.com",
                "zz.admin.example.com",
            ] {
                assert!(
                    regex_host_routes(position, "/api.*|.*admin/.example.com", hostname).is_some(),
                    "{position:?}: `.*` spans the label separator, so {hostname:?} routes",
                );
            }
        }

        // ... and the remediation that section recommends for `Pre`/`Post`.
        for position in [RulePosition::Pre, RulePosition::Post] {
            for (hostname, must_route) in [
                ("api.example.com", true),
                ("admin.example.com", true),
                ("apifoo.example.com", true),
                ("xadmin.example.com", true),
                ("api.foo.example.com", false),
                ("zz.admin.example.com", false),
            ] {
                assert_eq!(
                    regex_host_routes(position, "/api[^.]*|[^.]*admin/.example.com", hostname,)
                        .is_some(),
                    must_route,
                    "{position:?}: `[^.]*` spells the boundary out, {hostname:?}",
                );
            }
        }
    }

    /// The LITERAL-label arm of `convert_regex_domain_rule` split the anchors
    /// exactly as the regex-segment arm did, one `else` away from it: a label
    /// the operator never wrapped in slashes was pushed verbatim into the
    /// assembly, so a `|` in it was a live alternation. Measured on the tree
    /// as it stood, `/a/.x|y.com` assembled to `\A(?:a)\.x|y\.com\z` —
    /// `(\A(?:a)\.x)|(y\.com\z)` — and matched `a.xZZZ` and `ZZZy.com`.
    ///
    /// The fix is `regex::escape`, not a group. A literal label is a literal,
    /// so grouping would confine the alternation while leaving `+`, `?`, `*`,
    /// `^` and `$` live inside the same arm — `b+c` matching `bbbc` is the
    /// same defect with a different operator. Escaping is a no-op in matching
    /// behaviour for every label a real hostname can carry: an LDH label is
    /// `[A-Za-z0-9-]`, `escape` touches only the `-`, and `\-` is `-` outside
    /// a character class. That parity is asserted below rather than asserted
    /// of the hostname grammar in the abstract.
    ///
    /// To SEE THIS RED: drop the escaping in the literal arm of
    /// `convert_regex_domain_rule` — push `r` instead of `&regex::escape(r)`
    /// at both of its exits. The `a.xZZZ` row then fails with
    /// `left: true, right: false`. That mutation reddens THREE tests, not
    /// just this one: `convert_regex`'s two `*` rows go back to the
    /// unescaped spelling, and
    /// `a_star_label_is_a_wildcard_on_the_trie_and_a_literal_on_pre_and_post`
    /// loses the `Pre`/`Post` half of its table.
    #[test]
    fn a_literal_hostname_label_is_escaped_and_cannot_become_a_pattern() {
        // The three routing verdicts the defect turned on, through the
        // frontend surface that carries it.
        for (hostname, must_route) in [("a.x|y.com", true), ("a.xZZZ", false), ("ZZZy.com", false)]
        {
            assert_eq!(
                regex_host_routes(RulePosition::Pre, "/a/.x|y.com", hostname).is_some(),
                must_route,
                "literal label `x|y` against {hostname:?}",
            );
        }
        assert_eq!(
            convert_regex_domain_rule("/a/.x|y.com").expect("the hostname must convert"),
            "\\A(?:a)\\.x\\|y\\.com\\z",
            "a literal label must be escaped, not grouped and not pushed raw",
        );

        // The control, built by hand: the UNESCAPED assembly is what leaked.
        // No mutation of `convert_regex_domain_rule` can move this.
        let unescaped = Regex::new("\\A(?:a)\\.x|y\\.com\\z").expect("the raw assembly compiles");
        for anywhere in ["a.xZZZ", "ZZZy.com"] {
            assert!(
                unescaped.is_match(anywhere.as_bytes()),
                "{} leaks on {anywhere}",
                unescaped.as_str(),
            );
        }

        // Escaping must be inert for every label a hostname can actually
        // carry. `-` is the only LDH byte `regex::escape` touches, and `\-`
        // is `-` outside a character class.
        assert_eq!(
            convert_regex_domain_rule("/a/.my-host01.example.com")
                .expect("an LDH hostname must convert"),
            "\\A(?:a)\\.my\\-host01\\.example\\.com\\z",
            "escape only adds a backslash before the `-`",
        );
        for (hostname, must_route) in [
            ("a.my-host01.example.com", true),
            ("a.myXhost01.example.com", false),
            ("a.my-host01.example.comZZZ", false),
        ] {
            assert_eq!(
                regex_host_routes(RulePosition::Pre, "/a/.my-host01.example.com", hostname)
                    .is_some(),
                must_route,
                "escaped LDH label against {hostname:?}",
            );
        }

        // Grouping instead of escaping would have closed the `|` only. This
        // is why the arms are treated differently, measured rather than
        // argued: the same three quantifiers stay live under a group.
        let grouped_not_escaped =
            Regex::new("\\A(?:a)\\.(?:b+c)\\.com\\z").expect("the grouped assembly compiles");
        assert!(
            grouped_not_escaped.is_match(b"a.bbbc.com"),
            "{} still repeats the literal `b`",
            grouped_not_escaped.as_str(),
        );
        for (hostname, must_route) in [("a.b+c.com", true), ("a.bbbc.com", false)] {
            assert_eq!(
                regex_host_routes(RulePosition::Pre, "/a/.b+c.com", hostname).is_some(),
                must_route,
                "escaped literal label `b+c` against {hostname:?}",
            );
        }

        // Two deliberate behaviour changes, both narrowing, both stated in
        // `convert_regex_domain_rule`'s doc comment.
        //
        // (1) A literal `*` label was a quantifier over the preceding atom.
        // `\A*` repeats a zero-width assertion, which matches empty at any
        // offset, so the pattern was not anchored at its start at all.
        let star_unescaped =
            Regex::new("\\A*\\.(?:x)\\.example\\.com\\z").expect("the raw assembly compiles");
        assert!(
            star_unescaped.is_match(b"evil.attacker.x.example.com"),
            "{} is not anchored at its start",
            star_unescaped.as_str(),
        );
        assert_eq!(
            regex_host_routes(
                RulePosition::Pre,
                "*./x/.example.com",
                "evil.attacker.x.example.com",
            ),
            None,
            "a literal `*` label must no longer leave the pattern unanchored",
        );
        assert_eq!(
            regex_host_routes(RulePosition::Pre, "*./x/.example.com", "*.x.example.com").as_deref(),
            Some("REGEX-HOST"),
            "it matches the literal label the operator wrote, and only that",
        );

        // (2) A literal label carrying an unbalanced `(` or `[` made the
        // assembly fail to compile and the frontend was REJECTED. It is now
        // accepted and matches exactly the host that was typed. Assembled at
        // run time because the unbalanced literal is the point.
        for bracket in ["(", "["] {
            let hostname = format!("/a/.b{bracket}c.com");
            assert!(
                convert_regex_domain_rule(&hostname).is_some(),
                "{hostname} must convert",
            );
            assert_eq!(
                regex_host_routes(RulePosition::Pre, &hostname, &format!("a.b{bracket}c.com"))
                    .as_deref(),
                Some("REGEX-HOST"),
                "{hostname} must match the literal host it spells",
            );
            assert_eq!(
                regex_host_routes(RulePosition::Pre, &hostname, "a.bc.com"),
                None,
                "{hostname} must match nothing else",
            );
        }
    }

    /// `$HOST[n]` rewrite indices are read from `caps.iter().skip(1)` with
    /// the buffer sized by `captures_len()`, so the anchoring wrapper has to
    /// be NON-capturing: a capturing `(` … `)` shifts every index by one and
    /// silently rewrites traffic to the wrong target — a wrong `Host` header
    /// and a wrong backend, with nothing in the configuration to show for it.
    ///
    /// Both hostname code paths are asserted, because they read the capture
    /// from two DIFFERENT regexes that have to agree: a pre/post rule reads
    /// the whole-host regex `convert_regex_domain_rule` assembles
    /// (`RouteResult::new_no_trie`), a tree rule reads `pattern_trie`'s
    /// per-segment regex (`RouteResult::new_with_trie`) — while
    /// `capture_cap_host`, which bounds the template, is taken from the
    /// whole-host one in BOTH cases.
    ///
    /// To SEE THIS RED: make either wrapper capturing — `result.push_str("(")`
    /// instead of `"(?:"` in `convert_regex_domain_rule`, or
    /// `format!("\\A({segment})\\z")` in `pattern_trie.rs`'s
    /// `anchored_segment`. `$HOST[1]` then resolves to the whole segment
    /// `cdn42` instead of the operator's own group `42`, failing with
    /// `left: Some("cdn42.internal"), right: Some("42.internal")`.
    #[test]
    fn a_host_capture_rewrite_resolves_to_the_operators_own_group() {
        let rewritten_host = |position: RulePosition, template: &str, hostname: &str| {
            let mut front = test_http_frontend();
            front.hostname = "/cdn([0-9]+)/.example.com".to_owned();
            front.position = position;
            front.rewrite_host = Some(template.to_owned());
            let mut router = Router::new();
            router
                .add_http_front(&front)
                .unwrap_or_else(|error| panic!("{position:?} frontend must build: {error}"));
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.rewritten_host)
        };

        for position in [RulePosition::Pre, RulePosition::Post, RulePosition::Tree] {
            assert_eq!(
                rewritten_host(position, "$HOST[1].internal", "cdn42.example.com").as_deref(),
                Some("42.internal"),
                "{position:?}: $HOST[1] must be the operator's own group, not the wrapper's",
            );
            assert_eq!(
                rewritten_host(position, "$HOST[0]", "cdn42.example.com").as_deref(),
                Some("cdn42.example.com"),
                "{position:?}: $HOST[0] is the whole hostname",
            );
        }

        // The cap itself. `captures_len()` is 2, so index 2 does not exist
        // and the frontend must be refused loudly at registration rather
        // than rewriting to an empty string. A capturing wrapper would make
        // this index legal — which is exactly how a shifted index reaches
        // production unnoticed.
        let mut front = test_http_frontend();
        front.hostname = "/cdn([0-9]+)/.example.com".to_owned();
        front.rewrite_host = Some("$HOST[2]".to_owned());
        assert!(
            matches!(
                Router::new().add_http_front(&front),
                Err(RouterError::InvalidHostRewrite(_)),
            ),
            "a $HOST index past captures_len must be refused at registration",
        );
    }

    /// Grouping must not rescue a hostname the router rejects today. The
    /// wrapper supplies one `(` and one `)`, so an unbalanced segment can be
    /// balanced BY the wrapping — `a)(b` is not a regex, yet
    /// `\A(?:a)(b)\.example\.com\z` is, and it matches `ab.example.com`.
    /// Both hostname sites therefore compile the segment on its own before
    /// wrapping it, exactly as `PathRule::anchored_regex` does for a path.
    ///
    /// This covers that one direction. The converse is NOT guarded and is
    /// not asserted here: a segment that compiles bare can be rejected once
    /// wrapped. Such a hostname lands in `RouterError::InvalidDomain` at
    /// frontend registration, so it costs a loud rejection and not a
    /// mis-route.
    ///
    /// To SEE THIS RED: delete the `Regex::new(r).ok()?;` line in
    /// `convert_regex_domain_rule` — the first assertion then fails, the
    /// hostname builds, and `ab.example.com` routes. Delete the
    /// `Regex::new(segment).ok()?;` line in `pattern_trie.rs`'s
    /// `anchored_segment` instead and the `add_tree_rule` assertion fails.
    #[test]
    fn grouping_never_turns_a_rejected_hostname_regex_into_an_accepted_one() {
        // Assembled at run time rather than written as a literal because the
        // invalidity is the whole point and a literal invites a lint at the
        // `Regex::new` call site.
        let unbalanced = format!("/a{}b/.example.com", ")(");

        assert_eq!(
            convert_regex_domain_rule(&unbalanced),
            None,
            "{unbalanced} carries a segment that is not a regex on its own",
        );
        assert_eq!(
            unbalanced.parse::<DomainRule>(),
            Err(()),
            "{unbalanced} must not become a usable DomainRule",
        );
        assert!(
            !Router::new().add_tree_rule(
                unbalanced.as_bytes(),
                &PathRule::Prefix("/".to_owned()),
                &MethodRule::new(Some("GET".to_owned())),
                &Route::ClusterId("RESCUED".to_owned()),
            ),
            "{unbalanced} must not be storable in the trie either",
        );

        // The hazard is real, not hypothetical: the wrapping alone turns it
        // into a live pattern matching a host the operator never wrote.
        let rescued = Regex::new(&format!("\\A(?:a{}b)\\.example\\.com\\z", ")("))
            .expect("the wrapping balances the segment");
        assert!(
            rescued.is_match(b"ab.example.com"),
            "{} is what the missing pre-validation would have built",
            rescued.as_str(),
        );
    }

    /// One level up from the path rules, two overlapping regex HOSTNAME
    /// segments follow the same first-declared-wins rule: a trie node holds
    /// its regex segments in an ordered list, the lookup takes the first
    /// that matches AND serves the request, and a new segment is appended.
    /// Both segments below serve this request, so the first declared
    /// answers;
    /// `a_hostname_segment_whose_rules_reject_the_method_falls_through_to_the_next`
    /// covers the case where it does not.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_recursive`
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

    /// sozu#1351, shape one — the routing leak. An exact hostname added
    /// AFTER a regex segment that matches it used to attach its rule to
    /// the REGEX segment's leaf: `add_tree_rule` addresses the leaf
    /// through `domain_lookup_mut`, and that resolver walked a literal
    /// segment through the first regex segment matching it. The rule the
    /// operator wrote for `test4.example.com` was then served for every
    /// host `/test[0-9]/` matches, while `sozu query frontends` showed
    /// exactly what was typed.
    ///
    /// `TrieNode::lookup_mut` is key-addressed now: with
    /// `accept_wildcard: false` a literal key may not resolve into a
    /// non-literal entry (the guard `insert_sni_route` already documented
    /// for the wildcard slot, extended to the regex segments), so the add
    /// creates `test4.example.com`'s own node.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_mut`
    /// (`router/pattern_trie.rs`), drop the `accept_wildcard &&` guard in
    /// front of the `self.regexps` scan, so a literal segment resolves
    /// through a matching regex segment again. `test7` and `test9` then
    /// answer with EXACT-SPECIFIC.
    #[test]
    fn an_exact_host_added_after_a_matching_regex_segment_does_not_leak_to_the_family() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"/test[0-9]/.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("REGEX-FAMILY".to_owned()),
        ));
        assert!(router.add_tree_rule(
            b"test4.example.com",
            &PathRule::Prefix("/only-for-test4".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("EXACT-SPECIFIC".to_owned()),
        ));

        let resolve = |hostname: &str, path: &str| {
            router
                .lookup(hostname, path, &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve("test4.example.com", "/only-for-test4").as_deref(),
            Some("EXACT-SPECIFIC"),
            "the host the rule was written for must get it",
        );
        for leaked in ["test7.example.com", "test9.example.com"] {
            assert_eq!(
                resolve(leaked, "/only-for-test4").as_deref(),
                Some("REGEX-FAMILY"),
                "{leaked} must keep the family route and never reach the \
                 rule scoped to test4",
            );
        }
        assert_eq!(
            resolve("test4.example.com", "/").as_deref(),
            Some("REGEX-FAMILY"),
            "the exact node carries only /only-for-test4, so every other \
             path on that host falls through to the regex segment that \
             also claims it",
        );
    }

    /// sozu#1351 — the clearest statement of the same bug, and of its fix:
    /// declaring the exact host and the regex segment that matches it in
    /// either order must produce the SAME routing table. Before the fix
    /// the regex-first order attached the exact rule to the regex leaf
    /// (so `test7` answered with it and `test4/` answered at all), while
    /// the exact-first order built two separate nodes.
    ///
    /// To SEE THIS RED: same mutation as
    /// `an_exact_host_added_after_a_matching_regex_segment_does_not_leak_to_the_family`
    /// — drop the `accept_wildcard &&` guard in front of the
    /// `self.regexps` scan of `TrieNode::lookup_mut`. The two arrays then
    /// differ on `test4.example.com/` and on `test7.example.com`.
    #[test]
    fn hostname_routing_does_not_depend_on_declaration_order() {
        let exact: (&[u8], &str, &str) =
            (b"test4.example.com", "/only-for-test4", "EXACT-SPECIFIC");
        let regex: (&[u8], &str, &str) = (b"/test[0-9]/.example.com", "/", "REGEX-FAMILY");

        let routes = |declared: [(&[u8], &str, &str); 2]| {
            let mut router = Router::new();
            for (hostname, path, cluster) in declared {
                assert!(router.add_tree_rule(
                    hostname,
                    &PathRule::Prefix(path.to_owned()),
                    &MethodRule::new(None),
                    &Route::ClusterId(cluster.to_owned()),
                ));
            }
            [
                ("test4.example.com", "/only-for-test4"),
                ("test4.example.com", "/"),
                ("test7.example.com", "/only-for-test4"),
                ("test9.example.com", "/"),
            ]
            .map(|(hostname, path)| {
                router
                    .lookup(hostname, path, &Method::Get)
                    .ok()
                    .and_then(|result| result.cluster_id)
            })
        };

        let regex_first = routes([regex, exact]);
        let exact_first = routes([exact, regex]);
        assert_eq!(
            regex_first, exact_first,
            "hostname resolution must not depend on the order the \
             frontends were added in",
        );
        assert_eq!(
            regex_first[0].as_deref(),
            Some("EXACT-SPECIFIC"),
            "and the order-independent answer is the exact host's own rule",
        );
        assert_eq!(
            regex_first[2].as_deref(),
            Some("REGEX-FAMILY"),
            "while a sibling of the regex family never sees it",
        );
    }

    /// Hostname precedence is most-specific-first: an exact name beats a
    /// regex segment, a regex segment beats the `*` wildcard. One
    /// assertion per transition, since `doc/configure.md` states the order
    /// in prose.
    ///
    /// Regex-over-wildcard is the half that changed: `lookup_with_path`
    /// used to consult the wildcard slot BEFORE the regex list whenever
    /// the leftmost label was reached, so `*` outranked the narrower
    /// pattern.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_recursive`
    /// (`router/pattern_trie.rs`), move the wildcard block above the
    /// `self.regexps` loop. The REGEX-over-WILDCARD assertions then answer
    /// WILDCARD.
    #[test]
    fn hostname_precedence_is_exact_then_regex_then_wildcard() {
        let exact: &[u8] = b"test4.example.com";
        let regex: &[u8] = b"/test[0-9]/.example.com";
        let wildcard: &[u8] = b"*.example.com";

        let declare = |hostnames: &[(&[u8], &str)]| {
            let mut router = Router::new();
            for (hostname, cluster) in hostnames {
                assert!(router.add_tree_rule(
                    hostname,
                    &PathRule::Prefix("/".to_owned()),
                    &MethodRule::new(None),
                    &Route::ClusterId((*cluster).to_owned()),
                ));
            }
            router
                .lookup("test4.example.com", "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            declare(&[(regex, "REGEX"), (exact, "EXACT")]).as_deref(),
            Some("EXACT"),
            "an exact hostname outranks a regex segment that matches it",
        );
        assert_eq!(
            declare(&[(wildcard, "WILDCARD"), (exact, "EXACT")]).as_deref(),
            Some("EXACT"),
            "an exact hostname outranks the wildcard",
        );
        assert_eq!(
            declare(&[(wildcard, "WILDCARD"), (regex, "REGEX")]).as_deref(),
            Some("REGEX"),
            "a regex segment outranks the wildcard",
        );
        assert_eq!(
            declare(&[(regex, "REGEX"), (wildcard, "WILDCARD")]).as_deref(),
            Some("REGEX"),
            "and it outranks it in the other declaration order too",
        );
        assert_eq!(
            declare(&[(wildcard, "WILDCARD"), (regex, "REGEX"), (exact, "EXACT")]).as_deref(),
            Some("EXACT"),
            "with all three declared the most specific one answers",
        );
        assert_eq!(
            declare(&[(wildcard, "WILDCARD")]).as_deref(),
            Some("WILDCARD"),
            "the wildcard still answers when nothing narrower claims the host",
        );
    }

    /// The precedence order above is a search order, not a filter: a more
    /// specific hostname candidate that serves no rule for THIS request
    /// hands the request to the next one instead of ending the lookup.
    /// Without this, splitting a host's paths across an exact frontend and
    /// a regex family would make every path the exact frontend does not
    /// carry unroutable.
    ///
    /// To SEE THIS RED: in `TrieNode::lookup_recursive`
    /// (`router/pattern_trie.rs`), return
    /// `child.lookup_recursive(prefix, accept_wildcard, trace, accept)`
    /// directly from the `self.children.get(suffix)` arm instead of
    /// falling through on `None` — the pre-fix shape. `/regex-only` and
    /// `/served-by-neither` on `test4.example.com` then answer `None`.
    #[test]
    fn a_hostname_candidate_that_serves_no_rule_falls_through_to_the_next() {
        let mut router = Router::new();
        for (hostname, path, cluster) in [
            (&b"test4.example.com"[..], "/exact-only", "EXACT"),
            (&b"/test[0-9]/.example.com"[..], "/regex-only", "REGEX"),
            (&b"*.example.com"[..], "/", "WILDCARD"),
        ] {
            assert!(router.add_tree_rule(
                hostname,
                &PathRule::Prefix(path.to_owned()),
                &MethodRule::new(None),
                &Route::ClusterId(cluster.to_owned()),
            ));
        }

        let resolve = |hostname: &str, path: &str| {
            router
                .lookup(hostname, path, &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve("test4.example.com", "/exact-only").as_deref(),
            Some("EXACT"),
        );
        assert_eq!(
            resolve("test4.example.com", "/regex-only").as_deref(),
            Some("REGEX"),
            "the exact node carries no rule for this path, so the regex \
             segment that also claims the host answers",
        );
        assert_eq!(
            resolve("test4.example.com", "/served-by-neither").as_deref(),
            Some("WILDCARD"),
            "and when neither serves it the wildcard does",
        );
        assert_eq!(
            resolve("testA.example.com", "/regex-only").as_deref(),
            Some("WILDCARD"),
            "a host the regex segment does not match never reaches it",
        );
    }

    /// sozu#1351, shape two — the 404. Hostname candidates are chosen
    /// without looking at the request method, so the first segment to
    /// claim a host used to end the lookup even when its only rule was
    /// declared for another method. The second segment, which would have
    /// answered, was never consulted.
    ///
    /// The fallback above covers it: `Router::lookup` hands the trie the
    /// same (path, method) selection it will run on the winner, so a
    /// candidate that selects nothing is skipped.
    ///
    /// To SEE THIS RED: in `Router::lookup`, pass `&mut |_| true` as the
    /// `accept` predicate of `lookup_with_path` instead of
    /// `select_tree_rule(...).is_some()`. The GET below then answers
    /// `None` because the POST-only segment was declared first.
    #[test]
    fn a_hostname_segment_whose_rules_reject_the_method_falls_through_to_the_next() {
        let mut router = Router::new();
        // Both segments match `test4`; the POST-only one is declared first.
        assert!(router.add_tree_rule(
            b"/test[0-9]/.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("POST".to_owned())),
            &Route::ClusterId("POST-ONLY".to_owned()),
        ));
        assert!(router.add_tree_rule(
            b"/[a-z]+[0-9]/.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("GET-ONLY".to_owned()),
        ));

        let resolve = |method: &Method| {
            router
                .lookup("test4.example.com", "/", method)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve(&Method::Get).as_deref(),
            Some("GET-ONLY"),
            "the first-declared segment carries no GET rule, so the second \
             one answers instead of the request 404ing",
        );
        assert_eq!(
            resolve(&Method::Post).as_deref(),
            Some("POST-ONLY"),
            "and the first-declared segment still wins whenever it serves \
             the request",
        );
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

    // ── RFC 9110 §4.2.3: the Host is case-insensitive ──────────────────
    //
    // `add_tree_rule` (via `tree_hostname_to_ascii`) and
    // `DomainRule::from_str` both normalise a configured hostname
    // through `idna::domain_to_ascii`, which ASCII-lowercases (measured:
    // `"WWW.EXAMPLE.COM"` -> `"www.example.com"`, `"*.EXAMPLE.COM"` ->
    // `"*.example.com"`). A `/`-delimited regex segment is the one part
    // that is NOT normalised — see `tree_hostname_to_ascii` and
    // sozu#1377 — and is compiled case-insensitively instead, so an
    // uppercase literal inside it still meets the normalised key.
    // `Router::lookup` used to walk the
    // trie and the pre/post lists with the client's raw bytes, so the two
    // sides disagreed and no configuration could make an uppercase
    // `Host:` route. The tests below pin the restored invariant: add and
    // lookup normalise identically.

    /// To SEE THIS RED: in `Router::lookup`, replace
    /// `let normalized_hostname = normalize_hostname(hostname);` with
    /// `let normalized_hostname = Cow::Borrowed(hostname);`. Every
    /// uppercase and mixed-case assertion below then fails with
    /// `left: None, right: Some("CASE")` — the byte-exact trie walk the
    /// fix removed.
    #[test]
    fn an_uppercase_host_reaches_a_lowercase_tree_frontend() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("CASE".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        for host in [
            "www.example.com",
            "WWW.EXAMPLE.COM",
            "WwW.ExAmPlE.cOm",
            "www.EXAMPLE.com",
        ] {
            assert_eq!(
                resolve(host).as_deref(),
                Some("CASE"),
                "Host {host:?} names the same frontend as its lowercase form",
            );
        }
    }

    /// The other half of the issue: declaring the frontend in uppercase
    /// never was a workaround, because the add path lowercases it on the
    /// way in. Both spellings of the frontend must route both spellings
    /// of the Host.
    ///
    /// To SEE THIS RED: same mutation as
    /// `an_uppercase_host_reaches_a_lowercase_tree_frontend`.
    #[test]
    fn an_uppercase_tree_frontend_is_stored_and_looked_up_lowercase() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"WWW.EXAMPLE.COM",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("CASE".to_owned()),
        ));

        for host in ["www.example.com", "WWW.EXAMPLE.COM"] {
            assert_eq!(
                router
                    .lookup(host, "/", &Method::Get)
                    .ok()
                    .and_then(|result| result.cluster_id)
                    .as_deref(),
                Some("CASE"),
                "an uppercase frontend is stored lowercase, so {host:?} must resolve",
            );
        }
    }

    /// Wildcard and regex trie segments walk the same normalised key.
    /// The regex segment is the interesting one: its source is stored
    /// VERBATIM (`tree_hostname_to_ascii` normalises the literal labels
    /// only, because folding the source inverted `\D` into `\d` —
    /// sozu#1377), so an uppercase literal inside it meets the
    /// lowercased lookup key only because
    /// `pattern_trie::compiled_segment` compiles it case-insensitively.
    ///
    /// To SEE THIS RED: same mutation as
    /// `an_uppercase_host_reaches_a_lowercase_tree_frontend`. Dropping
    /// `.case_insensitive(true)` from `compiled_segment` reddens the
    /// `API7` assertion alone, which is the half this test also guards.
    #[test]
    fn an_uppercase_host_reaches_wildcard_and_regex_tree_segments() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"*.wild.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("WILDCARD".to_owned()),
        ));
        assert!(router.add_tree_rule(
            b"/API[0-9]/.rx.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("REGEX".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(resolve("FOO.WILD.EXAMPLE.COM").as_deref(), Some("WILDCARD"));
        assert_eq!(resolve("FoO.wild.EXAMPLE.com").as_deref(), Some("WILDCARD"));
        assert_eq!(
            resolve("API7.RX.EXAMPLE.COM").as_deref(),
            Some("REGEX"),
            "the operator's `/API[0-9]/` is stored verbatim and compiled \
             case-insensitively, so the normalised key matches it",
        );
        assert_eq!(resolve("api7.rx.example.com").as_deref(), Some("REGEX"));
    }

    /// Pre and post rules carry a `DomainRule` rather than a trie node.
    /// `Exact` and `Wildcard` are lowercased by `DomainRule::from_str`
    /// (again `idna::domain_to_ascii`) and compared byte-exact, so they
    /// had exactly the tree's asymmetry.
    ///
    /// To SEE THIS RED: same mutation as
    /// `an_uppercase_host_reaches_a_lowercase_tree_frontend`.
    #[test]
    fn an_uppercase_host_reaches_exact_and_wildcard_pre_post_rules() {
        let mut router = Router::new();
        let exact = "pre.example.com"
            .parse::<DomainRule>()
            .expect("an exact domain rule must parse");
        let wildcard = "*.post.example.com"
            .parse::<DomainRule>()
            .expect("a wildcard domain rule must parse");
        assert!(router.add_pre_rule(
            &exact,
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("PRE".to_owned()),
        ));
        assert!(router.add_post_rule(
            &wildcard,
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("POST".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(resolve("PRE.EXAMPLE.COM").as_deref(), Some("PRE"));
        assert_eq!(resolve("PrE.ExAmPlE.cOm").as_deref(), Some("PRE"));
        assert_eq!(resolve("FOO.POST.EXAMPLE.COM").as_deref(), Some("POST"));
        assert_eq!(resolve("foo.post.example.com").as_deref(), Some("POST"));
    }

    /// The interaction that makes normalising the lookup key dangerous on
    /// its own: a pre/post hostname REGEX keeps the operator's bytes —
    /// `convert_regex_domain_rule` copies the segment source verbatim,
    /// as the trie now does too (`tree_hostname_to_ascii`, sozu#1377;
    /// up to and including 2.2.1 `domain_to_ascii` lowercased it there).
    /// Matching a
    /// normalised key against a case-sensitive `/API[0-9]/` would have
    /// turned a rule that used to match `API7.…` into one that matches
    /// nothing. `DomainRule::from_str` therefore compiles hostname
    /// regexes case-insensitively, which is also the only reading
    /// that agrees with the trie: `pattern_trie::compiled_segment`
    /// folds the same way, on the same stored bytes.
    ///
    /// To SEE THIS RED: in `DomainRule::from_str`, drop the
    /// `.case_insensitive(true)` from the `RegexBuilder` and build with
    /// `regex::bytes::Regex::new(&s)` again. Both assertions below fail
    /// with `left: None, right: Some("RX")`.
    #[test]
    fn an_uppercase_pre_rule_hostname_regex_still_matches_its_own_host() {
        let mut router = Router::new();
        let uppercase_regex = "/API[0-9]/.example.com"
            .parse::<DomainRule>()
            .expect("an uppercase hostname regex must parse");
        assert!(router.add_pre_rule(
            &uppercase_regex,
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("RX".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve("API7.example.com").as_deref(),
            Some("RX"),
            "the spelling the operator wrote must keep routing",
        );
        assert_eq!(
            resolve("api7.example.com").as_deref(),
            Some("RX"),
            "and its canonical lowercase form, which never routed before",
        );
    }

    /// The regression guard for the `.unicode(false)` that
    /// [`DomainRule::from_str`] deliberately does NOT set. Setting it
    /// would make the case folding ASCII-only — which is what the
    /// hostnames ever reaching `matches` are anyway — but it also
    /// refuses Unicode-class syntax at compile time, and `\p{L}` matches
    /// ASCII letters perfectly well, so a frontend that installs and
    /// routes today would start being rejected at add time.
    ///
    /// To SEE THIS RED: add `.unicode(false)` to the `RegexBuilder` in
    /// `DomainRule::from_str`. The parse fails and the `expect` fires.
    #[test]
    fn a_unicode_class_hostname_regex_still_compiles_and_routes() {
        let mut router = Router::new();
        let unicode_class = "/\\p{L}+[0-9]/.example.com"
            .parse::<DomainRule>()
            .expect("a hostname regex using a Unicode class must still compile");
        assert!(router.add_pre_rule(
            &unicode_class,
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("UNICODE".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve("api7.example.com").as_deref(),
            Some("UNICODE"),
            "a Unicode letter class covers ASCII letters, so this rule is live, not decorative",
        );
        assert_eq!(
            resolve("API7.example.com").as_deref(),
            Some("UNICODE"),
            "and it stays live for the uppercase spelling of the same host",
        );
    }

    /// Normalisation and the hostname precedence order of sozu#1351
    /// compose in one direction only: the key is normalised first, then
    /// the most-specific-first search runs on it, so case never decides
    /// WHICH candidate wins. Stated in `doc/configure.md` under
    /// "Hostname case", so it needs an assertion.
    ///
    /// The shape is the one the precedence section uses: an exact name
    /// and a regex segment that also claims it. The exact frontend must
    /// win for both spellings — a normalisation applied anywhere after
    /// the candidate search would let the uppercase spelling miss the
    /// exact node and fall through to the regex family.
    ///
    /// To SEE THIS RED: in `Router::lookup`, replace
    /// `let normalized_hostname = normalize_hostname(hostname);` with
    /// `let normalized_hostname = Cow::Borrowed(hostname);`. The
    /// uppercase spelling stops resolving at all.
    #[test]
    fn case_normalisation_runs_before_the_hostname_precedence_search() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"test4.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("EXACT".to_owned()),
        ));
        assert!(router.add_tree_rule(
            b"/test[0-9]/.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("REGEX-FAMILY".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve("test4.example.com").as_deref(),
            Some("EXACT"),
            "the exact name outranks a regex segment that also claims it",
        );
        for spelling in ["TEST4.EXAMPLE.COM", "TeSt4.ExAmPlE.cOm"] {
            assert_eq!(
                resolve(spelling).as_deref(),
                Some("EXACT"),
                "{spelling:?} must pick the same tier as its lowercase form, \
                 not fall through to the regex family",
            );
        }

        // A host only the regex family claims still reaches it in either
        // spelling: normalising did not collapse the tiers, it fed them.
        assert_eq!(
            resolve("test7.example.com").as_deref(),
            Some("REGEX-FAMILY")
        );
        assert_eq!(
            resolve("TEST7.EXAMPLE.COM").as_deref(),
            Some("REGEX-FAMILY")
        );
    }

    /// `has_hostname` decides whether a removed frontend's cached tags may
    /// be dropped. It is fed the CONFIGURED hostname, which the pre/post
    /// arms then compared byte-exact against a `DomainRule` the add path
    /// had already lowercased — so an uppercase frontend answered `false`
    /// while its route was still installed, and the tags went away under a
    /// live route.
    ///
    /// To SEE THIS RED: in `has_hostname`, replace
    /// `let normalized_hostname = normalize_hostname(hostname);` with
    /// `let normalized_hostname = Cow::Borrowed(hostname);`. The two
    /// uppercase assertions fail.
    #[test]
    fn has_hostname_answers_for_an_uppercase_spelling() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"tree.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("TREE".to_owned()),
        ));
        assert!(
            router.add_pre_rule(
                &"pre.example.com"
                    .parse::<DomainRule>()
                    .expect("an exact domain rule must parse"),
                &PathRule::Prefix("/".to_owned()),
                &MethodRule::new(None),
                &Route::ClusterId("PRE".to_owned()),
            )
        );

        assert!(router.has_hostname("TREE.EXAMPLE.COM"));
        assert!(router.has_hostname("PRE.EXAMPLE.COM"));
        assert!(!router.has_hostname("ABSENT.EXAMPLE.COM"));
    }

    /// `$HOST[n]` captures are taken from the key the router matched on,
    /// so a rewrite template now emits the normalised host rather than
    /// the client's raw bytes. That is the intended reading — the
    /// configured frontend is itself stored lowercase — and it is stated
    /// in `doc/configure.md`, so it needs an assertion.
    ///
    /// To SEE THIS RED: same mutation as
    /// `an_uppercase_host_reaches_a_lowercase_tree_frontend`; the rewrite
    /// stops resolving at all (`RouteNotFound`) and the `expect` fires.
    #[test]
    fn a_host_rewrite_capture_sees_the_normalized_host() {
        let mut router = Router::new();
        router
            .add_http_front(&HttpFrontend {
                hostname: "rewrite.example.com".to_owned(),
                rewrite_host: Some("$HOST[0].internal".to_owned()),
                ..test_http_frontend()
            })
            .expect("the rewrite frontend must install");

        let route = router
            .lookup("REWRITE.EXAMPLE.COM", "/", &Method::Get)
            .expect("an uppercase Host must resolve");
        assert_eq!(
            route.rewritten_host.as_deref(),
            Some("rewrite.example.com.internal"),
            "`$HOST[0]` carries the normalised host, not the client's bytes",
        );
    }

    /// The normalisation must cost a scan and not an allocation on the
    /// overwhelmingly common path — every conforming client already sends
    /// a lowercase Host, and this runs once per request.
    ///
    /// To SEE THIS RED: make `normalize_hostname` return
    /// `Cow::Owned(hostname.to_ascii_lowercase())` unconditionally. The
    /// first assertion fails.
    #[test]
    fn normalize_hostname_borrows_an_already_lowercase_host() {
        assert!(
            matches!(normalize_hostname("www.example.com"), Cow::Borrowed(_)),
            "a lowercase host must not allocate",
        );
        assert!(
            matches!(normalize_hostname("123.example-host.com"), Cow::Borrowed(_)),
            "digits and hyphens are not uppercase and must not allocate",
        );
        assert_eq!(
            normalize_hostname("WwW.ExAmPlE.cOm"),
            Cow::Owned::<str>("www.example.com".to_owned()),
        );
    }

    /// Known gap, deliberately NOT closed here: RFC 1034 §3.1 makes
    /// `www.example.com.` the absolute form of `www.example.com`, but
    /// `idna::domain_to_ascii` keeps the trailing dot on the add path
    /// (measured: `"www.example.com."` -> `"www.example.com."`) and the
    /// trie then splits it into a trailing empty label that no
    /// dot-free frontend carries. The TLS side already strips one
    /// trailing dot (`lib/src/https.rs`'s `sni_owned`,
    /// `lib/src/protocol/tcp_preread/mod.rs`'s `normalize_sni`); HTTP
    /// routing does not. This test records the current answer so the day
    /// someone fixes it, it fails here and gets inverted rather than
    /// silently changing behaviour.
    ///
    /// To SEE THIS RED: strip a single trailing `.` inside
    /// `normalize_hostname`.
    #[test]
    fn an_absolute_form_host_does_not_reach_a_relative_frontend_yet() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            b"www.example.com",
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(Some("GET".to_owned())),
            &Route::ClusterId("CASE".to_owned()),
        ));

        assert!(
            router
                .lookup("www.example.com.", "/", &Method::Get)
                .is_err(),
            "absolute-form Host routing is a separate defect, tracked \
             apart from the case asymmetry",
        );
        assert!(
            router
                .lookup("WWW.EXAMPLE.COM.", "/", &Method::Get)
                .is_err(),
            "normalising the case must not accidentally close it either",
        );
    }

    // ── sozu#1377: IDNA folding must not reach a regex segment ─────────
    //
    // `add_tree_rule`, `remove_tree_rule` and `has_hostname` used to run
    // the WHOLE configured hostname through `idna::domain_to_ascii`,
    // which ASCII-lowercases (measured: `"/\\D+/.example.com"` ->
    // `"/\\d+/.example.com"`, `"/[^\\D]/.example.com"` ->
    // `"/[^\\d]/.example.com"`, `"/API[0-9]/.rx.example.com"` ->
    // `"/api[0-9]/.rx.example.com"`). Folding an uppercase regex escape
    // INVERTS the character class it names -- `\D` is "not a digit",
    // `\d` is "a digit"; `\W`/`\w` and `\S`/`\s` invert the same way --
    // so the stored rule matched the exact COMPLEMENT of what the
    // operator wrote while `sozu query frontends` still echoed the
    // original spelling.
    //
    // `Pre`/`Post` never carried it: `convert_regex_domain_rule` copies a
    // segment's source verbatim and `DomainRule::from_str` takes its case
    // insensitivity from the `RegexBuilder` instead. The tree now agrees:
    // `tree_hostname_to_ascii` normalises only the literal labels and
    // `pattern_trie::compiled_segment` folds case at compile time.

    /// To SEE THIS RED: in `tree_hostname_to_ascii`, replace the whole
    /// body with `::idna::domain_to_ascii(hostname).ok()`, the form the
    /// three call sites carried up to 2.2.1. Both assertions then fail
    /// INVERTED -- `abc.rx.example.com` resolves to `None` and
    /// `777.rx.example.com` to `Some("NOT-A-DIGIT")` -- which is the
    /// defect itself: the rule matches the complement of its source --
    /// `\D` matching a digit.
    #[test]
    fn a_tree_hostname_regex_escape_is_not_case_folded() {
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            "/\\D+/.rx.example.com".as_bytes(),
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("NOT-A-DIGIT".to_owned()),
        ));

        let resolve = |hostname: &str| {
            router
                .lookup(hostname, "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
        };

        assert_eq!(
            resolve("777.rx.example.com"),
            None,
            "`\\D+` must NOT match a run of digits; folded to `\\d+` it matched \
             exactly this and nothing else",
        );
        assert_eq!(
            resolve("abc.rx.example.com").as_deref(),
            Some("NOT-A-DIGIT"),
            "`\\D+` is \"one or more non-digits\" and `abc` is three of them",
        );
    }

    /// The negated-class spelling from the report, and the two other
    /// uppercase escapes that invert: `\W` (not a word character) and
    /// `\S` (not whitespace). `[^\D]` is "not a non-digit", i.e. a digit;
    /// folded to `[^\d]` it becomes its own complement.
    ///
    /// To SEE THIS RED: same mutation as
    /// `a_tree_hostname_regex_escape_is_not_case_folded`. Every
    /// `matches` assertion below fails, each with its complement.
    #[test]
    fn every_inverting_uppercase_regex_escape_survives_a_tree_insert() {
        // (segment source, a host label it must match, one it must not)
        for (segment, hit, miss) in [
            ("[^\\D]+", "777", "abc"),
            ("\\W+", "---", "abc"),
            ("\\S+", "abc", "   "),
        ] {
            let mut router = Router::new();
            let hostname = format!("/{segment}/.rx.example.com");
            assert!(
                router.add_tree_rule(
                    hostname.as_bytes(),
                    &PathRule::Prefix("/".to_owned()),
                    &MethodRule::new(None),
                    &Route::ClusterId("ESCAPE".to_owned()),
                ),
                "{hostname} must install",
            );

            let resolve = |hostname: &str| {
                router
                    .lookup(hostname, "/", &Method::Get)
                    .ok()
                    .and_then(|result| result.cluster_id)
            };

            assert_eq!(
                resolve(&format!("{hit}.rx.example.com")).as_deref(),
                Some("ESCAPE"),
                "{segment} must match {hit}",
            );
            assert_eq!(
                resolve(&format!("{miss}.rx.example.com")),
                None,
                "{segment} must not match {miss}",
            );
        }
    }

    /// The other half of the split: a LITERAL label is still a domain
    /// label and still goes through IDNA, in the very same hostname whose
    /// regex segment is copied verbatim. Only a `/`-delimited segment is
    /// exempt -- the same split `convert_regex_domain_rule` and the trie's
    /// `insert_recursive` already make.
    ///
    /// `remove_tree_rule` normalises through the same helper, so the rule
    /// stays addressable: a second spelling of the stored key would make
    /// the frontend permanently un-removable.
    ///
    /// To SEE THIS RED: same mutation as
    /// `a_tree_hostname_regex_escape_is_not_case_folded`. The first
    /// assertion fails with `left: None, right: Some("IDN")` -- the
    /// punycode is right, the folded `\d+` segment is what misses.
    #[test]
    fn a_unicode_label_is_punycoded_while_its_sibling_regex_segment_is_not() {
        let hostname = "/\\D+/.MÜNCHEN.example.com";
        let mut router = Router::new();
        assert!(router.add_tree_rule(
            hostname.as_bytes(),
            &PathRule::Prefix("/".to_owned()),
            &MethodRule::new(None),
            &Route::ClusterId("IDN".to_owned()),
        ));

        // `idna::domain_to_ascii("MÜNCHEN")` -> `"xn--mnchen-3ya"`
        // (measured, and identical label-by-label and whole-domain).
        assert_eq!(
            router
                .lookup("abc.xn--mnchen-3ya.example.com", "/", &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id)
                .as_deref(),
            Some("IDN"),
            "the literal label must still be punycoded and lowercased",
        );

        assert!(
            router.remove_tree_rule(
                hostname.as_bytes(),
                &PathRule::Prefix("/".to_owned()),
                &MethodRule::new(None),
            ),
            "the frontend must still be addressable by the spelling that \
             installed it",
        );
        assert!(
            router
                .lookup("abc.xn--mnchen-3ya.example.com", "/", &Method::Get)
                .is_err(),
            "and removing it must actually unroute it",
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // Property-test harness: sozu#1349, sozu#1351, sozu#1356, sozu#1377
    // ═══════════════════════════════════════════════════════════════════
    //
    // Every regression test above pins ONE fixed example. This harness
    // generates many, against an ORACLE that is independent of the trie:
    // it does not call `pattern_trie::anchored_segment`,
    // `compiled_segment`, `TrieNode::lookup*`, `tree_hostname_to_ascii` or
    // `convert_regex_domain_rule` — reusing any of those would make the
    // oracle agree with the implementation by construction, proving
    // nothing. It is written fresh from `doc/configure.md`'s "Hostname
    // precedence" and "Regex hostname segments" sections and the RFC text
    // those sections cite (RFC 9110 §4.2.3 for case, RFC 9110's own
    // grammar for the wildcard), and it uses the `regex` crate only as a
    // primitive — exactly as `pattern_trie` does — never the router's own
    // wrapping of it.
    //
    // Scope, stated explicitly because it is a deliberate narrowing: every
    // generated rule and every query share the fixed two-label suffix
    // `example.com`, one variable FRONT label, and `MethodRule::new(None)`
    // (matches every method). This is what puts every rule in a scenario
    // on the SAME trie branch, which is where sozu#1351's leak and
    // precedence questions live, and it removes METHOD precedence from
    // the oracle's job — a separate concern with its own existing tests
    // (`case_normalisation_runs_before_the_hostname_precedence_search`
    // and neighbours). PATH does vary — see `PATH_POOL` — because sozu#1351's
    // actual production symptom needs it: with every rule pinned to the
    // SAME path, a literal whose insert would leak onto a regex leaf finds
    // that leaf already carrying an identical `(path, method)`, so
    // `add_tree_rule`'s append-skip condition is always false and the
    // insert is refused before any lookup happens — the leak is real but
    // observable only as `add_tree_rule` returning `false`, never as a
    // routing mismatch. Varying path is what lets the SAME leak surface the
    // way it did in production: an exact rule at one path answering a
    // request for a hostname nobody declared, at a DIFFERENT declared path.
    //
    // The generator is biased, not uniform: `REGEX_CATALOG` is the fixed
    // set of segment shapes that actually bit sozu, `LITERAL_POOL`
    // deliberately overlaps those shapes' `hits` so an exact rule and a
    // regex rule naturally collide on the same host (sozu#1351), case is
    // varied on both the declared rule and the query independently
    // (sozu#1349), declaration order is an explicit Fisher-Yates shuffle
    // driven by the same `Gen` (sozu#1351 again — order must not move an
    // exact-vs-regex verdict, and must move a regex-vs-regex one), and
    // queries cross every generated hit/miss hostname against every path
    // declared anywhere in the scenario, not just the declaring rule's own
    // (sozu#1351's leak crosses fronts, so a query pairing has to as well).
    //
    // Two honesty notes about this harness's own limits, not the router's:
    //
    // - **No reproducible seed.** `quickcheck` 1.1.0's `Gen::new` seeds
    //   from OS entropy with no seed exposed to reproduce a specific run,
    //   unlike the FoundationDB-style simulators `doc/testing.md` §5
    //   describes (`SOZU_UDP_SIM_SEED` and friends). A failing CI run here
    //   is reproduced by RE-RUNNING locally with a raised `QUICKCHECK_TESTS`
    //   (this property's generator is dense enough that a real regression
    //   reappears quickly), not by replaying the failing seed — there is
    //   none to replay. Not fixed in this changeset.
    // - **The anchoring convention is shared, not external.** This oracle's
    //   `\A(?:source)\z` case-insensitive anchoring matches
    //   `doc/configure.md`'s "Regex hostname segments" section — but that
    //   section's prose was itself ADDED by the sozu#1356 fix this harness
    //   is supposed to catch a regression of. So for the GROUPING half of
    //   that anchoring, the oracle and the implementation share a
    //   convention rather than the oracle checking against an independent
    //   spec; a regression that un-groups BOTH the doc and the code back to
    //   their pre-#1356 form would not be caught by prose comparison alone
    //   — it is caught here because the oracle's grouping is also
    //   independently reasoned from what `|`'s precedence in a regex
    //   grammar requires, the same reasoning #1356's own fix commit gives.
    //   The CASE-FOLDING half (`(?i)`) is genuinely independent, derived
    //   straight from RFC 9110 §4.2.3, not from anything #1356 or #1377
    //   added to `doc/configure.md`.

    /// One `/regex/` hostname segment source this harness declares, with
    /// concrete labels independently reasoned to match or not match it
    /// under `\A(?:source)\z` compiled case-insensitively — the anchoring
    /// `doc/configure.md` documents and sozu#1356 restored, built fresh
    /// here rather than borrowed from `anchored_segment`.
    struct RegexCase {
        source: &'static str,
        /// Whole-label matches for one branch of the pattern.
        hits: &'static [&'static str],
        /// Labels that must NOT match under the documented semantics, but
        /// that a pre-fix implementation matched: a half-anchored
        /// alternation branch (sozu#1356 — `axx`, `xxb`, the bare
        /// substring `zzbzz` for a 3-way alternation's ungrouped middle
        /// branch) or the inverted class an ASCII-lowercase fold produces
        /// (sozu#1377 — folding `\D` into `\d` turns "not a digit" into
        /// "a digit").
        misses: &'static [&'static str],
    }

    const REGEX_CATALOG: &[RegexCase] = &[
        RegexCase {
            source: "a|b",
            hits: &["a", "b"],
            misses: &["axx", "xxb", "xx"],
        },
        RegexCase {
            source: "a|b|c",
            // The 3-branch case two branches cannot express: ungrouped,
            // the MIDDLE branch of `\Aa|b|c\z` keeps NEITHER anchor and
            // matches as a bare substring anywhere in the label.
            hits: &["a", "b", "c"],
            misses: &["axx", "xxc", "zzbzz", "bzz", "zzb"],
        },
        RegexCase {
            source: "api|admin",
            // `doc/configure.md`'s own worked example for sozu#1356.
            hits: &["api", "admin"],
            misses: &["apifoo", "xadmin"],
        },
        RegexCase {
            source: "\\D+",
            // sozu#1377's own measured shape: folded to `\d+`, this
            // matched digits and rejected letters — the complement.
            hits: &["abc", "xyz"],
            misses: &["777", "42"],
        },
        RegexCase {
            source: "\\W+",
            hits: &["---", "..."],
            misses: &["abc", "42a"],
        },
        RegexCase {
            source: "\\S+",
            hits: &["abc", "777"],
            // Folded to `\s`, this would match ONLY whitespace; no ASCII
            // hostname label the wire ever admits can carry one, but
            // `Router::lookup` takes a plain `&str` and does not itself
            // re-validate the charset, so the harness can still probe it.
            misses: &[" "],
        },
        RegexCase {
            source: "[^\\D]+",
            // sozu#1377's negated-class shape from the issue itself:
            // "not a non-digit" is a digit; folded it becomes its own
            // complement.
            hits: &["777", "42"],
            misses: &["abc"],
        },
        RegexCase {
            source: "API[0-9]",
            // An uppercase literal INSIDE a class: must still meet the
            // lowercased lookup key by folding at compile time, not by
            // folding the stored source (which would invert the escapes
            // above in the very same pass).
            hits: &["api7", "API7", "ApI4"],
            misses: &["apiZ"],
        },
        RegexCase {
            source: "test[0-9]",
            // sozu#1351's own shape: `test4` is also `LITERAL_POOL`'s
            // exact-rule text, so a literal `test4` rule and this regex
            // rule collide on the same host with decent probability.
            hits: &["test4", "TEST7", "Test9"],
            misses: &["testA", "test10x"],
        },
        RegexCase {
            source: "[a-z]+[0-9]",
            hits: &["test4", "cdn1"],
            misses: &["4test", "TEST"],
        },
    ];

    /// Literal exact-rule candidates. Deliberately overlapping with
    /// `REGEX_CATALOG`'s `hits` — `test4`, `api`, `admin`, `a`, `b`, `c`
    /// all reappear there — so an exact rule and an overlapping regex
    /// rule land in the SAME generated scenario often enough to exercise
    /// sozu#1351 without hand-injecting a forced pair.
    const LITERAL_POOL: &[&str] = &[
        "a", "b", "c", "api", "admin", "test4", "test7", "test9", "TEST4", "API7", "abc", "777",
        "42", "cdn1", "zz", "apifoo", "xadmin",
    ];

    /// Declared-path candidates. Small and deliberately NOT elaborate: the
    /// production shape of sozu#1351 needs exactly two rules at two
    /// different paths (reproduced live against the reverted fix: an exact
    /// `test4.example.com` frontend at `/other` leaking onto the
    /// `/test[0-9]/.example.com` regex family, answering `test7.example.com`
    /// requests for `/other`). `"/"` is a prefix of every other entry here
    /// and is weighted to appear often, so a scenario still frequently
    /// declares two fronts at the SAME path (exercising the harness's
    /// pre-existing hostname-only coverage) as well as at different ones
    /// (exercising the path dimension this fixes). Every entry is used only
    /// as a `PathRule::Prefix`; `PathRule::Equals`/`PathRule::Regex` are not
    /// generated — see the module doc comment for why method stays fixed
    /// too, and the same reasoning applies here: sozu#1351 needs two
    /// declared paths to differ, not the full path-matching precedence
    /// surface, which has its own existing tests.
    const PATH_POOL: &[&str] = &["/", "/", "/other", "/api", "/only"];

    /// The dot-separated FRONT label of a generated Tree-position
    /// hostname rule. Every rule in a [`Scenario`] shares the fixed
    /// trailing `example.com` — see the harness's module-level doc
    /// comment for why.
    #[derive(Clone, Copy, Debug)]
    enum FrontLabel {
        /// A literal label, in whatever case the generator chose.
        /// `tree_hostname_to_ascii` case-folds it on insert (sozu#1377).
        Literal(&'static str),
        /// The leftmost-only wildcard, exactly one label.
        Wildcard,
        /// A `/regex/` segment, addressed into [`REGEX_CATALOG`].
        Regex(usize),
    }

    /// Dedup identity for a [`FrontLabel`] within one scenario. Two
    /// literals that differ only by case must be treated as the SAME
    /// front: `add_tree_rule` case-folds them to the same trie node, and
    /// generating both would silently drop one rule's cluster id from the
    /// harness's own model — a harness bug, not something under test.
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum FrontKey {
        Literal(String),
        Wildcard,
        Regex(usize),
    }

    impl From<FrontLabel> for FrontKey {
        fn from(front: FrontLabel) -> Self {
            match front {
                FrontLabel::Literal(text) => FrontKey::Literal(text.to_ascii_lowercase()),
                FrontLabel::Wildcard => FrontKey::Wildcard,
                FrontLabel::Regex(idx) => FrontKey::Regex(idx),
            }
        }
    }

    #[derive(Clone, Debug)]
    struct GenRule {
        front: FrontLabel,
        /// This rule's OWN declared path, from [`PATH_POOL`]. Every front
        /// carries exactly one `(path, route)` pair in this harness's
        /// model — one `add_tree_rule` call per generated front — so there
        /// is no within-one-hostname path-precedence list to model; only
        /// whether THIS front's path serves a given query path.
        path: &'static str,
        cluster: String,
    }

    /// One generated case: a set of Tree-position rules, in DECLARATION
    /// order (already shuffled — see [`Scenario::arbitrary`]), and a set
    /// of (hostname, path) queries to look up against them.
    #[derive(Clone, Debug)]
    struct Scenario {
        rules: Vec<GenRule>,
        queries: Vec<(String, String)>,
    }

    fn render_hostname(front: FrontLabel) -> String {
        match front {
            FrontLabel::Literal(text) => format!("{text}.example.com"),
            FrontLabel::Wildcard => "*.example.com".to_owned(),
            FrontLabel::Regex(idx) => format!("/{}/.example.com", REGEX_CATALOG[idx].source),
        }
    }

    /// The oracle's OWN anchoring: `\A(?:source)\z`, case-insensitive.
    /// Built fresh from `doc/configure.md`'s "Regex hostname segments"
    /// section and the sozu#1356 fix's stated invariant, not by calling
    /// `pattern_trie::anchored_segment` or `compiled_segment`. Using the
    /// `regex` crate as a primitive is not reusing the implementation:
    /// regex matching itself is not what any of the four issues broke —
    /// Sozu's own anchoring, grouping and case handling around it is.
    fn oracle_regex_matches(source: &str, label: &str) -> bool {
        let pattern = format!("(?i)\\A(?:{source})\\z");
        Regex::new(&pattern)
            .unwrap_or_else(|error| {
                panic!("the harness's own catalog regex {source:?} must compile: {error}")
            })
            .is_match(label.as_bytes())
    }

    fn front_label_matches(front: FrontLabel, query_label: &str) -> bool {
        match front {
            FrontLabel::Literal(text) => text.eq_ignore_ascii_case(query_label),
            FrontLabel::Wildcard => !query_label.is_empty(),
            FrontLabel::Regex(idx) => oracle_regex_matches(REGEX_CATALOG[idx].source, query_label),
        }
    }

    /// Whether `front` (this harness's stand-in for one declared Tree
    /// rule) matches the full hostname `query` — case-insensitively on
    /// the fixed `example.com` suffix (RFC 9110 §4.2.3, sozu#1349) and
    /// per [`front_label_matches`] on the one variable label.
    fn oracle_hostname_matches(front: FrontLabel, query: &str) -> bool {
        let mut labels = query.split('.');
        let (Some(first), Some(second), Some(third), None) =
            (labels.next(), labels.next(), labels.next(), labels.next())
        else {
            return false;
        };
        second.eq_ignore_ascii_case("example")
            && third.eq_ignore_ascii_case("com")
            && front_label_matches(front, first)
    }

    /// `PathRule::Prefix` semantics (`PathRule::matches`, `router/mod.rs`):
    /// a rule matches iff the query path starts with the declared path's
    /// bytes. Every generated front carries exactly one path, so there is
    /// no longest-prefix tie-break to model here — see [`GenRule::path`].
    fn oracle_path_matches(rule_path: &str, query_path: &str) -> bool {
        query_path.starts_with(rule_path)
    }

    /// The oracle's precedence search, stated in `doc/configure.md` under
    /// "Hostname precedence" and restored by sozu#1351: the EXACT name,
    /// then a REGEX segment (the first declared among those that match),
    /// then the `*` WILDCARD — each candidate accepted only when it ALSO
    /// serves this request's path (`Router::lookup` hands the trie the
    /// same path-serving predicate it uses on the winner, so a hostname
    /// candidate that matches but does not serve this path is skipped
    /// rather than ending the search — `doc/configure.md`, "Hostname
    /// precedence": "a search, not a filter"). Exact-vs-regex hostname
    /// precedence is checked in a pass that does not depend on `rules`'
    /// order at all — the documented, order-independent half of the
    /// invariant sozu#1351 restored. Regex-vs-regex precedence is "first
    /// in `rules`' own order that matches (hostname AND path)" — the half
    /// that DOES still depend on declaration order, by design
    /// (`doc/configure.md`, "Regex hostname segments").
    fn oracle_lookup<'r>(rules: &'r [GenRule], hostname: &str, path: &str) -> Option<&'r str> {
        let serves = |rule: &&GenRule| {
            oracle_hostname_matches(rule.front, hostname) && oracle_path_matches(rule.path, path)
        };
        rules
            .iter()
            .find(|rule| matches!(rule.front, FrontLabel::Literal(_)) && serves(rule))
            .or_else(|| {
                rules
                    .iter()
                    .find(|rule| matches!(rule.front, FrontLabel::Regex(_)) && serves(rule))
            })
            .or_else(|| {
                rules
                    .iter()
                    .find(|rule| matches!(rule.front, FrontLabel::Wildcard) && serves(rule))
            })
            .map(|rule| rule.cluster.as_str())
    }

    /// Vary the case of `s`: as declared, all-uppercase, all-lowercase, or
    /// a per-character random mix — independently of whatever case the
    /// OTHER side (rule vs. query) picked, so RFC 9110 §4.2.3
    /// case-insensitivity (sozu#1349) is exercised on both sides and not
    /// just one.
    fn case_variant(g: &mut Gen, s: &str) -> String {
        match u8::arbitrary(g) % 4 {
            0 => s.to_owned(),
            1 => s.to_ascii_uppercase(),
            2 => s.to_ascii_lowercase(),
            _ => s
                .chars()
                .map(|c| {
                    if bool::arbitrary(g) {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect(),
        }
    }

    fn push_unique_front(
        front: FrontLabel,
        path: &'static str,
        rules: &mut Vec<GenRule>,
        seen: &mut Vec<FrontKey>,
    ) {
        let key = FrontKey::from(front);
        if seen.contains(&key) {
            return;
        }
        seen.push(key);
        let cluster = format!("C{}", rules.len());
        rules.push(GenRule {
            front,
            path,
            cluster,
        });
    }

    /// One hit and one miss hostname sample per declared front (see
    /// [`RegexCase`]'s doc comment for what those pin), plus three
    /// universal hostname probes every scenario gets regardless of what it
    /// declared: an unrelated host that must match nothing, the bare
    /// suffix with the front label dropped entirely (must miss even a
    /// declared wildcard, which stands for exactly one label and not
    /// zero), and its uppercase spelling.
    ///
    /// Each hit hostname is queried at EVERY distinct path declared
    /// anywhere in the scenario, not just its own front's path — sozu#1351's
    /// leak crosses fronts (an exact rule's cluster answering a DIFFERENT
    /// hostname, at whatever path THAT hostname's own rule declared), so a
    /// query pairing that only ever asked a front's own path could not
    /// observe it. Miss hostnames and the universal probes are checked only
    /// at their own front's path (or `/`): they exist to confirm a mismatch
    /// does not appear where none is expected, which does not need the
    /// cross product.
    fn push_hit_queries(
        g: &mut Gen,
        label: &str,
        declared_paths: &[&str],
        queries: &mut Vec<(String, String)>,
    ) {
        for &path in declared_paths {
            queries.push((
                format!("{}.example.com", case_variant(g, label)),
                path.to_owned(),
            ));
        }
    }

    fn generate_queries(g: &mut Gen, rules: &[GenRule]) -> Vec<(String, String)> {
        let mut queries = Vec::new();
        let mut declared_paths: Vec<&str> = rules.iter().map(|rule| rule.path).collect();
        declared_paths.sort_unstable();
        declared_paths.dedup();

        for rule in rules {
            match rule.front {
                FrontLabel::Literal(text) => {
                    push_hit_queries(g, text, &declared_paths, &mut queries);
                }
                FrontLabel::Wildcard => {
                    let label = *g.choose(LITERAL_POOL).unwrap_or(&"zz");
                    push_hit_queries(g, label, &declared_paths, &mut queries);
                }
                FrontLabel::Regex(idx) => {
                    let case = &REGEX_CATALOG[idx];
                    if let Some(&hit) = g.choose(case.hits) {
                        push_hit_queries(g, hit, &declared_paths, &mut queries);
                    }
                    if let Some(miss) = g.choose(case.misses) {
                        queries.push((
                            format!("{}.example.com", case_variant(g, miss)),
                            rule.path.to_owned(),
                        ));
                    }
                }
            }
        }

        queries.push((
            format!("{}.example.com", case_variant(g, "unmatched-host")),
            "/".to_owned(),
        ));
        queries.push(("example.com".to_owned(), "/".to_owned()));
        queries.push(("EXAMPLE.COM".to_owned(), "/".to_owned()));

        queries
    }

    impl Arbitrary for Scenario {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut rules = Vec::new();
            let mut seen_keys = Vec::new();

            let pick_path = |g: &mut Gen| {
                *g.choose(PATH_POOL)
                    .expect("PATH_POOL is a non-empty const slice")
            };

            // With even odds, include the wildcard: it participates in
            // every precedence chain and costs only one extra rule.
            if bool::arbitrary(g) {
                let path = pick_path(g);
                push_unique_front(FrontLabel::Wildcard, path, &mut rules, &mut seen_keys);
            }

            // 1..=3 additional fronts, each independently a regex segment
            // or a literal drawn from the overlap-biased pool, each with
            // its own independently chosen path.
            let extra = 1 + (u8::arbitrary(g) % 3);
            for _ in 0..extra {
                if bool::arbitrary(g) {
                    let idx = usize::from(u8::arbitrary(g)) % REGEX_CATALOG.len();
                    let path = pick_path(g);
                    push_unique_front(FrontLabel::Regex(idx), path, &mut rules, &mut seen_keys);
                } else {
                    let text = *g
                        .choose(LITERAL_POOL)
                        .expect("LITERAL_POOL is a non-empty const slice");
                    let path = pick_path(g);
                    push_unique_front(FrontLabel::Literal(text), path, &mut rules, &mut seen_keys);
                }
            }

            // Declaration order is itself under test (sozu#1351): shuffle
            // with a Fisher-Yates driven by the same `Gen`, independent of
            // insertion order above.
            for i in (1..rules.len()).rev() {
                let j = usize::from(u8::arbitrary(g)) % (i + 1);
                rules.swap(i, j);
            }

            let queries = generate_queries(g, &rules);
            Scenario { rules, queries }
        }

        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            let mut shrunk = Vec::new();

            for i in 0..self.rules.len() {
                let mut rules = self.rules.clone();
                rules.remove(i);
                shrunk.push(Scenario {
                    rules,
                    queries: self.queries.clone(),
                });
            }

            for i in 0..self.queries.len() {
                let mut queries = self.queries.clone();
                queries.remove(i);
                shrunk.push(Scenario {
                    rules: self.rules.clone(),
                    queries,
                });
            }

            Box::new(shrunk.into_iter())
        }
    }

    /// Every catalog entry's own `hits`/`misses` verdicts, checked against
    /// nothing but [`oracle_regex_matches`] — a self-check that the
    /// harness's table is internally consistent BEFORE it is trusted to
    /// judge the router. This does not touch `Router` or `TrieNode` at
    /// all.
    #[test]
    fn regex_catalog_samples_match_their_own_documented_semantics() {
        for case in REGEX_CATALOG {
            for hit in case.hits {
                assert!(
                    oracle_regex_matches(case.source, hit),
                    "{:?} must match {hit:?} under \\A(?:...)\\z case-insensitive",
                    case.source,
                );
            }
            for miss in case.misses {
                assert!(
                    !oracle_regex_matches(case.source, miss),
                    "{:?} must NOT match {miss:?} under \\A(?:...)\\z case-insensitive",
                    case.source,
                );
            }
        }
    }

    /// Build the rules `scenario` declares into a fresh [`Router`], in
    /// its own declaration order, then check every query against both the
    /// oracle and the router.
    fn oracle_matches_router(scenario: Scenario) -> TestResult {
        if scenario.rules.is_empty() {
            return TestResult::discard();
        }

        let mut router = Router::new();
        for rule in &scenario.rules {
            let hostname = render_hostname(rule.front);
            // A refused insert here is not automatically dismissed as a
            // harness artifact: sozu#1351's own mechanism (a literal
            // resolving through a matching regex segment because
            // `TrieNode::lookup_mut`'s guard is gone) can make this EXACT
            // call return `false`, when the two fronts' generated paths
            // happen to coincide -- see the module doc comment. Every
            // front here is still unique (`FrontKey`-deduped) and every
            // catalog regex compiles standalone, so a refusal is real
            // signal either way: fail loudly and report it rather than
            // silently skipping the rule, which would just hide the
            // question inside a `None` at lookup time instead of at
            // insert time.
            if !router.add_tree_rule(
                hostname.as_bytes(),
                &PathRule::Prefix(rule.path.to_owned()),
                &MethodRule::new(None),
                &Route::ClusterId(rule.cluster.clone()),
            ) {
                eprintln!(
                    "INSERT REFUSED hostname={hostname:?} path={:?} cluster={:?} \
                     declared-so-far={:?}",
                    rule.path,
                    rule.cluster,
                    scenario
                        .rules
                        .iter()
                        .map(|r| (render_hostname(r.front), r.path))
                        .collect::<Vec<_>>(),
                );
                return TestResult::failed();
            }
        }

        for (hostname, path) in &scenario.queries {
            let expected = oracle_lookup(&scenario.rules, hostname, path);
            let actual = router
                .lookup(hostname, path, &Method::Get)
                .ok()
                .and_then(|result| result.cluster_id);

            if expected != actual.as_deref() {
                let declared: Vec<(String, &str)> = scenario
                    .rules
                    .iter()
                    .map(|rule| (render_hostname(rule.front), rule.path))
                    .collect();
                eprintln!(
                    "MISMATCH host={hostname:?} path={path:?} expected={expected:?} \
                     actual={actual:?} declared (in order, with path)={declared:?}",
                );
                return TestResult::failed();
            }
        }

        TestResult::passed()
    }

    quickcheck! {
        fn qc_router_hostname_resolution_matches_the_documented_semantics(scenario: Scenario) -> TestResult {
            oracle_matches_router(scenario)
        }
    }
}
