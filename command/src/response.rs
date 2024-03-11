use std::{cmp::Ordering, collections::BTreeMap, fmt, net::SocketAddr};

use crate::{
    proto::command::{
        AddBackend, FilteredTimeSerie, LoadBalancingParams, PathRule, PathRuleKind,
        RequestHttpFrontend, RequestTcpFrontend, Response, ResponseContent, ResponseStatus,
        RulePosition, RunState, WorkerResponse,
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
}

impl From<HttpFrontend> for RequestHttpFrontend {
    fn from(val: HttpFrontend) -> Self {
        let tags = match val.tags {
            Some(tags) => tags,
            None => BTreeMap::new(),
        };
        RequestHttpFrontend {
            cluster_id: val.cluster_id,
            address: val.address.into(),
            hostname: val.hostname,
            path: val.path,
            method: val.method,
            position: val.position.into(),
            tags,
        }
    }
}

impl From<Backend> for AddBackend {
    fn from(val: Backend) -> Self {
        AddBackend {
            cluster_id: val.cluster_id,
            backend_id: val.backend_id,
            address: val.address.into(),
            sticky_id: val.sticky_id,
            load_balancing_parameters: val.load_balancing_parameters,
            backup: val.backup,
        }
    }
}

impl PathRule {
    pub fn prefix<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Prefix.into(),
            value: value.to_string(),
        }
    }

    pub fn regex<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Regex.into(),
            value: value.to_string(),
        }
    }

    pub fn equals<S>(value: S) -> Self
    where
        S: ToString,
    {
        Self {
            kind: PathRuleKind::Equals.into(),
            value: value.to_string(),
        }
    }

    pub fn from_cli_options(
        path_prefix: Option<String>,
        path_regex: Option<String>,
        path_equals: Option<String>,
    ) -> Self {
        match (path_prefix, path_regex, path_equals) {
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
        }
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
        RequestTcpFrontend {
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
        self.cluster_id
            .cmp(&o.cluster_id)
            .then(self.backend_id.cmp(&o.backend_id))
            .then(self.sticky_id.cmp(&o.sticky_id))
            .then(
                self.load_balancing_parameters
                    .cmp(&o.load_balancing_parameters),
            )
            .then(self.backup.cmp(&o.backup))
            .then(socketaddr_cmp(&self.address, &o.address))
    }
}

impl PartialOrd for Backend {
    fn partial_cmp(&self, other: &Backend) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Backend {
    pub fn to_add_backend(self) -> AddBackend {
        AddBackend {
            cluster_id: self.cluster_id,
            address: self.address.into(),
            sticky_id: self.sticky_id,
            backend_id: self.backend_id,
            load_balancing_parameters: self.load_balancing_parameters,
            backup: self.backup,
        }
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Serialize)]
struct StatePath {
    path: String,
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
            &self.last_minute[0..10], &self.last_minute[10..20], &self.last_minute[20..30], &self.last_minute[30..40], &self.last_minute[40..50], &self.last_minute[50..60],
            &self.last_hour[0..10], &self.last_hour[10..20], &self.last_hour[20..30], &self.last_hour[30..40], &self.last_hour[40..50], &self.last_hour[50..60])
    }
}

fn socketaddr_cmp(a: &SocketAddr, b: &SocketAddr) -> Ordering {
    a.ip().cmp(&b.ip()).then(a.port().cmp(&b.port()))
}
