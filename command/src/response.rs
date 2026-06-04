use std::{cmp::Ordering, collections::BTreeMap, fmt, net::SocketAddr};

use crate::{
    proto::command::{
        AddBackend, FilteredTimeSerie, Header, HstsConfig, LoadBalancingParams, PathRule,
        PathRuleKind, RequestHttpFrontend, RequestTcpFrontend, RequestUdpFrontend, Response,
        ResponseContent, ResponseStatus, RulePosition, RunState, WorkerResponse,
    },
    state::ClusterId,
};

impl Response {
    pub fn new(
        status: ResponseStatus,
        message: String,
        content: Option<ResponseContent>,
    ) -> Response {
        Response {
            status: status as i32,
            message,
            content,
        }
    }
}

/// An HTTP or HTTPS frontend, as used *within* Sōzu
#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HttpFrontend {
    /// Send a 401, DENY, if cluster_id is None
    pub cluster_id: Option<ClusterId>,
    pub address: SocketAddr,
    pub hostname: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "is_default_path_rule")]
    pub path: PathRule,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub position: RulePosition,
    pub tags: Option<BTreeMap<String, String>>,
    /// Resolved frontend-level policy carried over from
    /// [`RequestHttpFrontend`]. The router consults these to build a
    /// [`Route::Frontend(Rc<Frontend>)`] when any are non-default,
    /// otherwise falls back to the legacy `Route::ClusterId` /
    /// `Route::Deny` shapes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect: Option<i32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_scheme: Option<i32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_template: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite_host: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite_path: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite_port: Option<u32>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_auth: Option<bool>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    /// Resolved per-frontend HSTS (RFC 6797) policy. `None` means inherit
    /// the listener default at frontend-add time in the worker.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hsts: Option<HstsConfig>,
}

impl From<HttpFrontend> for RequestHttpFrontend {
    fn from(val: HttpFrontend) -> Self {
        let source_address = val.address;
        let source_hostname = val.hostname.clone();
        let request_frontend = RequestHttpFrontend {
            cluster_id: val.cluster_id,
            address: val.address.into(),
            hostname: val.hostname,
            path: val.path,
            method: val.method,
            position: val.position.into(),
            tags: val.tags.unwrap_or_default(),
            redirect: val.redirect,
            redirect_scheme: val.redirect_scheme,
            redirect_template: val.redirect_template,
            rewrite_host: val.rewrite_host,
            rewrite_path: val.rewrite_path,
            rewrite_port: val.rewrite_port,
            required_auth: val.required_auth,
            headers: val.headers,
            hsts: val.hsts,
        };

        // POST: the proto-encoded address decodes back to the source SocketAddr
        // and the hostname is unchanged — the in-Sōzu → wire conversion must be
        // routing-preserving (the SocketAddress ⇔ SocketAddr round-trip is
        // exercised in request.rs; here we tie the frontend identity to it).
        debug_assert_eq!(
            SocketAddr::from(request_frontend.address),
            source_address,
            "frontend address must round-trip through the proto encoding"
        );
        debug_assert_eq!(
            request_frontend.hostname, source_hostname,
            "frontend hostname must survive the proto conversion"
        );
        request_frontend
    }
}

impl From<Backend> for AddBackend {
    fn from(val: Backend) -> Self {
        let source_address = val.address;
        let source_cluster_id = val.cluster_id.clone();
        let source_backend_id = val.backend_id.clone();
        let add_backend = AddBackend {
            cluster_id: val.cluster_id,
            backend_id: val.backend_id,
            address: val.address.into(),
            sticky_id: val.sticky_id,
            load_balancing_parameters: val.load_balancing_parameters,
            backup: val.backup,
        };

        // POST: backend identity (cluster + backend id) and the wire address
        // are preserved — a backend that changed cluster/id/address here would
        // be registered under the wrong key and never receive (or steal)
        // traffic.
        debug_assert_eq!(
            add_backend.cluster_id, source_cluster_id,
            "backend cluster_id must survive the proto conversion"
        );
        debug_assert_eq!(
            add_backend.backend_id, source_backend_id,
            "backend_id must survive the proto conversion"
        );
        debug_assert_eq!(
            SocketAddr::from(add_backend.address),
            source_address,
            "backend address must round-trip through the proto encoding"
        );
        add_backend
    }
}

impl PathRule {
    pub fn prefix<S>(value: S) -> Self
    where
        S: ToString,
    {
        let rule = Self {
            kind: PathRuleKind::Prefix.into(),
            value: value.to_string(),
        };
        // POST: the encoded kind decodes back to the Prefix variant — the proto
        // i32 must round-trip or the router would misclassify the match type.
        debug_assert_eq!(
            PathRuleKind::try_from(rule.kind),
            Ok(PathRuleKind::Prefix),
            "prefix() must encode a Prefix-kind rule"
        );
        rule
    }

    pub fn regex<S>(value: S) -> Self
    where
        S: ToString,
    {
        let rule = Self {
            kind: PathRuleKind::Regex.into(),
            value: value.to_string(),
        };
        debug_assert_eq!(
            PathRuleKind::try_from(rule.kind),
            Ok(PathRuleKind::Regex),
            "regex() must encode a Regex-kind rule"
        );
        rule
    }

    pub fn equals<S>(value: S) -> Self
    where
        S: ToString,
    {
        let rule = Self {
            kind: PathRuleKind::Equals.into(),
            value: value.to_string(),
        };
        debug_assert_eq!(
            PathRuleKind::try_from(rule.kind),
            Ok(PathRuleKind::Equals),
            "equals() must encode an Equals-kind rule"
        );
        rule
    }

    pub fn from_cli_options(
        path_prefix: Option<String>,
        path_regex: Option<String>,
        path_equals: Option<String>,
    ) -> Self {
        // PRE: prefix takes precedence over regex, which takes precedence over
        // equals. Capture which arms are populated so the post-condition can
        // assert the precedence actually fired.
        let had_prefix = path_prefix.is_some();
        let had_regex = path_regex.is_some();
        let rule = match (path_prefix, path_regex, path_equals) {
            (Some(prefix), _, _) => PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: prefix,
            },
            (None, Some(regex), _) => PathRule {
                kind: PathRuleKind::Regex as i32,
                value: regex,
            },
            (None, None, Some(equals)) => PathRule {
                kind: PathRuleKind::Equals as i32,
                value: equals,
            },
            _ => PathRule::default(),
        };

        // POST: a present prefix wins outright; absent a prefix, a present
        // regex wins. The resolved kind must reflect that precedence so two
        // simultaneously-set CLI flags can never silently pick the wrong rule.
        debug_assert!(
            !had_prefix || rule.kind == PathRuleKind::Prefix as i32,
            "a path prefix must produce a Prefix rule regardless of other flags"
        );
        debug_assert!(
            had_prefix || !had_regex || rule.kind == PathRuleKind::Regex as i32,
            "absent a prefix, a regex must produce a Regex rule"
        );
        rule
    }
}

pub fn is_default_path_rule(p: &PathRule) -> bool {
    PathRuleKind::try_from(p.kind) == Ok(PathRuleKind::Prefix) && p.value.is_empty()
}

impl fmt::Display for PathRule {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match PathRuleKind::try_from(self.kind) {
            Ok(PathRuleKind::Prefix) => write!(f, "prefix '{}'", self.value),
            Ok(PathRuleKind::Regex) => write!(f, "regexp '{}'", self.value),
            Ok(PathRuleKind::Equals) => write!(f, "equals '{}'", self.value),
            Err(_) => write!(f, ""),
        }
    }
}

/// A TCP frontend, as used *within* Sōzu
#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TcpFrontend {
    pub cluster_id: String,
    pub address: SocketAddr,
    /// custom tags to identify the frontend in the access logs
    pub tags: BTreeMap<String, String>,
}

impl From<TcpFrontend> for RequestTcpFrontend {
    fn from(val: TcpFrontend) -> Self {
        let source_address = val.address;
        let source_cluster_id = val.cluster_id.clone();
        let request_frontend = RequestTcpFrontend {
            cluster_id: val.cluster_id,
            address: val.address.into(),
            tags: val.tags,
        };

        // POST: cluster identity and the wire address are preserved across the
        // proto conversion (same routing guarantee as the HTTP frontend path).
        debug_assert_eq!(
            request_frontend.cluster_id, source_cluster_id,
            "TCP frontend cluster_id must survive the proto conversion"
        );
        debug_assert_eq!(
            SocketAddr::from(request_frontend.address),
            source_address,
            "TCP frontend address must round-trip through the proto encoding"
        );
        request_frontend
    }
}

/// A UDP frontend, as used *within* Sōzu
#[derive(Debug, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UdpFrontend {
    pub cluster_id: String,
    pub address: SocketAddr,
    /// custom tags to identify the frontend in the access logs
    pub tags: BTreeMap<String, String>,
}

impl From<UdpFrontend> for RequestUdpFrontend {
    fn from(val: UdpFrontend) -> Self {
        RequestUdpFrontend {
            cluster_id: val.cluster_id,
            address: val.address.into(),
            tags: val.tags,
        }
    }
}

/// A backend, as used *within* Sōzu
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Backend {
    pub cluster_id: String,
    pub backend_id: String,
    pub address: SocketAddr,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sticky_id: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_balancing_parameters: Option<LoadBalancingParams>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<bool>,
}

impl Ord for Backend {
    fn cmp(&self, o: &Backend) -> Ordering {
        // INV: Equal can only be returned when every keyed field compares
        // Equal — the tuple of field orderings is the source of truth, and the
        // `.then(...)` fold must not collapse two distinct backends to Equal
        // (which would let one silently evict the other from a BTree key set).
        // Computed inline (no recursion into `cmp`) so it stays cheap.
        let fields_all_equal = self.cluster_id == o.cluster_id
            && self.backend_id == o.backend_id
            && self.sticky_id == o.sticky_id
            && self.load_balancing_parameters == o.load_balancing_parameters
            && self.backup == o.backup
            && self.address == o.address;

        let ordering = self
            .cluster_id
            .cmp(&o.cluster_id)
            .then(self.backend_id.cmp(&o.backend_id))
            .then(self.sticky_id.cmp(&o.sticky_id))
            .then(
                self.load_balancing_parameters
                    .cmp(&o.load_balancing_parameters),
            )
            .then(self.backup.cmp(&o.backup))
            .then(socketaddr_cmp(&self.address, &o.address));

        debug_assert_eq!(
            ordering == Ordering::Equal,
            fields_all_equal,
            "Backend::cmp returns Equal iff every keyed field is equal"
        );
        ordering
    }
}

impl PartialOrd for Backend {
    fn partial_cmp(&self, other: &Backend) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Backend {
    pub fn to_add_backend(self) -> AddBackend {
        let source_address = self.address;
        let source_backend_id = self.backend_id.clone();
        let add_backend = AddBackend {
            cluster_id: self.cluster_id,
            address: self.address.into(),
            sticky_id: self.sticky_id,
            backend_id: self.backend_id,
            load_balancing_parameters: self.load_balancing_parameters,
            backup: self.backup,
        };

        // POST: identity (backend id) and wire address are preserved — same
        // routing guarantee as the `From<Backend>` path, kept in lockstep.
        debug_assert_eq!(
            add_backend.backend_id, source_backend_id,
            "backend_id must survive to_add_backend"
        );
        debug_assert_eq!(
            SocketAddr::from(add_backend.address),
            source_address,
            "backend address must round-trip through to_add_backend"
        );
        add_backend
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

pub type MessageId = String;

impl WorkerResponse {
    pub fn ok<T>(id: T) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            message: String::new(),
            status: ResponseStatus::Ok.into(),
            content: None,
        }
    }

    pub fn ok_with_content<T>(id: T, content: ResponseContent) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            status: ResponseStatus::Ok.into(),
            message: String::new(),
            content: Some(content),
        }
    }

    pub fn error<T, U>(id: T, error: U) -> Self
    where
        T: ToString,
        U: ToString,
    {
        Self {
            id: id.to_string(),
            message: error.to_string(),
            status: ResponseStatus::Failure.into(),
            content: None,
        }
    }

    pub fn processing<T>(id: T) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            message: String::new(),
            status: ResponseStatus::Processing.into(),
            content: None,
        }
    }

    pub fn with_status<T>(id: T, status: ResponseStatus) -> Self
    where
        T: ToString,
    {
        Self {
            id: id.to_string(),
            message: String::new(),
            status: status.into(),
            content: None,
        }
    }

    pub fn is_failure(&self) -> bool {
        self.status == ResponseStatus::Failure as i32
    }
}

impl fmt::Display for WorkerResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{:?}", self.id, self.status)
    }
}

impl fmt::Display for FilteredTimeSerie {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "FilteredTimeSerie {{\nlast_second: {},\nlast_minute:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\nlast_hour:\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n}}",
            self.last_second,
            &self.last_minute[0..10],
            &self.last_minute[10..20],
            &self.last_minute[20..30],
            &self.last_minute[30..40],
            &self.last_minute[40..50],
            &self.last_minute[50..60],
            &self.last_hour[0..10],
            &self.last_hour[10..20],
            &self.last_hour[20..30],
            &self.last_hour[30..40],
            &self.last_hour[40..50],
            &self.last_hour[50..60]
        )
    }
}

fn socketaddr_cmp(a: &SocketAddr, b: &SocketAddr) -> Ordering {
    let ordering = a.ip().cmp(&b.ip()).then(a.port().cmp(&b.port()));
    // INV: two socket addresses compare Equal iff both IP and port match —
    // the `.then` fold must not declare distinct (ip, port) pairs equal, which
    // would make two different backends indistinguishable to the BTree key.
    debug_assert_eq!(
        ordering == Ordering::Equal,
        a.ip() == b.ip() && a.port() == b.port(),
        "socketaddr_cmp is Equal iff ip and port both match"
    );
    ordering
}
