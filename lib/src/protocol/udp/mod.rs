//! Pure sans-io UDP load-balancing core.
//!
//! This module is the *pure* heart of the UDP datapath (issue #1273). It owns
//! admission, the virtual 4-tuple flow table, the three-knob teardown
//! state-machine, the LB-selection request protocol, and a single-deadline
//! timer scheduler with generation tokens. It performs **no I/O**: there is no
//! socket, no `Instant::now()` / `SystemTime`, no `rand`, and no `Arc<Mutex>`.
//! Time is injected as `now: Instant` parameters; the hash seed is injected at
//! construction. The single admission copy the design allows materialises the
//! borrowed recv buffer into an owned `Vec<u8>` ([`Transmit::payload`]).
//!
//! The I/O shell ([`crate::udp`]) owns every syscall, the buffer pool, the timer
//! wheel, the connected per-flow upstream sockets, the `BackendMap`, health
//! checks and metrics. It drives this core through [`ManagerInput`] / [`Output`].
//!
//! Two-level split (mirrors the H2 `ConnectionH2` / `Context` split in
//! `protocol/mux/`):
//! - [`UdpManager`](manager::UdpManager): flow table + admission + cap/shedding
//!   + flow-key extraction + LB request + timer scheduling.
//! - [`UdpFlow`](flow::UdpFlow): per-admitted-flow teardown counters, idle /
//!   lifetime deadlines, PPv2 bookkeeping, chosen backend, forward/return
//!   decisions, and a `timer_gen`.
//!
//! Long-form lifecycle (flow state machine, NAT return, teardown, hardening):
//! `lib/src/protocol/udp/LIFECYCLE.md`.

pub mod flow;
pub mod manager;
pub mod proxy_protocol;

use std::{net::SocketAddr, time::Duration};

use sozu_command::state::ClusterId;

pub use crate::protocol::udp::{
    flow::{CloseReason, UdpFlow},
    manager::{FlowKeyExtractor, SourceTupleExtractor, UdpManager},
};

/// Slab index of an admitted flow. Stable for the flow's lifetime; reused after
/// close. The shell maps `upstream_token -> FlowId` for the NAT return path.
pub type FlowId = usize;

/// Backend identifier, mirroring [`sozu_command::state`]'s string backend ids
/// and `Backend::backend_id` in `lib/src/backends.rs`.
pub type BackendId = String;

/// The virtual flow key extracted from a client datagram. The default
/// [`SourceTupleExtractor`] keys on the real (pre-NAT) client source address;
/// `with_port` distinguishes the 2-tuple (source IP only) from the 4-tuple
/// (source IP + port). Other extractors may key differently — the trait is the
/// only seam — but the 4-tuple impl is the only one in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FlowKey {
    /// The client source address. When the extractor keys on source IP only,
    /// the port is normalised to `0`.
    pub src: SocketAddr,
}

impl FlowKey {
    /// Build a key from a client source address, keeping the port when
    /// `with_port` is set, normalising it to `0` otherwise.
    pub fn from_src(src: SocketAddr, with_port: bool) -> Self {
        if with_port {
            FlowKey { src }
        } else {
            let mut src = src;
            src.set_port(0);
            FlowKey { src }
        }
    }
}

/// An owned datagram ready to be written by the shell. `payload` is the single
/// admission copy (`Vec<u8>`); for the first upstream datagram of a PPv2 flow
/// the core has already prepended the v2 DGRAM header, so the shell writes
/// `payload` verbatim. `segment_size` reserves room for a future GSO/GRO fast
/// path and is `None` in phase 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transmit {
    /// Destination address (backend for upstream, real client for return).
    pub dst: SocketAddr,
    /// GSO segment size hint; `None` in phase 1 (no batching).
    pub segment_size: Option<usize>,
    /// Owned datagram bytes, PPv2-prefixed in place when applicable.
    pub payload: Vec<u8>,
}

/// Inputs the shell feeds into the manager. Borrows the recv buffer; the core
/// copies into an owned `Vec<u8>` only on admission.
#[derive(Debug)]
pub enum ManagerInput<'a> {
    /// A datagram from a client. Admitted into an existing/new flow or dropped.
    ClientDatagram {
        /// Real (pre-NAT) client source address.
        src: SocketAddr,
        /// Borrowed datagram bytes.
        payload: &'a [u8],
    },
    /// A datagram from a backend, tagged by the shell with the owning flow
    /// (`upstream_token -> FlowId`). Drives the symmetric NAT return path.
    BackendDatagram {
        /// Flow that owns the connected upstream socket this arrived on.
        flow: FlowId,
        /// Borrowed datagram bytes.
        payload: &'a [u8],
    },
    /// A control-plane / health event (add/remove backend, LB algo, timeouts,
    /// per-cluster knobs, drain). Never allocates a flow.
    Config(ConfigEvent),
    /// Reply to an earlier [`Output::SelectBackend`]: the shell resolved the
    /// backend via the `BackendMap` and is committing it to the flow.
    BackendResolved {
        /// Flow awaiting a backend.
        flow: FlowId,
        /// Resolved backend identifier.
        backend: BackendId,
        /// Resolved backend address for the connected upstream socket.
        addr: SocketAddr,
    },
}

/// Outputs the manager emits; the shell drains them via `poll_output` until
/// `None`. The shell owns the actual syscalls and timer wheel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// A new flow needs a backend. The shell consults the `BackendMap` with
    /// `(cluster, key)` and replies with [`ManagerInput::BackendResolved`].
    SelectBackend {
        /// Flow awaiting a backend.
        flow: FlowId,
        /// Cluster the flow's listener routes to.
        cluster: ClusterId,
        /// Affinity hash for HRW / Maglev selection (`key % M`, `max hash`).
        key: u64,
    },
    /// The shell should `connect()` a fresh upstream socket for this flow and
    /// register `upstream_token -> flow` for NAT return demux.
    OpenUpstream {
        /// Flow that owns the upstream socket.
        flow: FlowId,
        /// Backend address to connect to.
        backend: SocketAddr,
    },
    /// Write an owned datagram to the backend (PPv2-prefixed when applicable).
    SendToBackend(Transmit),
    /// Write an owned datagram back to the real client (symmetric NAT return).
    SendToClient(Transmit),
    /// Re-arm the single manager-wide timer at this absolute deadline. The
    /// shell owns the wheel; the core only ever requests one deadline.
    ArmTimer(std::time::Instant),
    /// A metric event for the shell to translate into `udp.*` counters/gauges.
    Metric(MetricEvent),
    /// Tear down a flow (idle / responses-reached / requests-reached / drain).
    /// The shell closes the upstream socket, frees the slab slot, and
    /// decrements `udp.active_flows`.
    CloseFlow(FlowId),
    /// A datagram was dropped before allocating any flow/buffer/socket
    /// ("silence is a virtue"). Carries the reason for metrics/logs.
    Drop(DropReason),
}

/// Why a datagram was dropped. Maps onto `udp.datagrams.dropped` by-reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// Datagram failed validation (empty, or the extractor rejected it).
    Invalid,
    /// Datagram exceeded the configured `max_rx_datagram_size` (truncation /
    /// `MSG_TRUNC` surrogate at this layer).
    Truncated,
    /// No cluster/backend is configured for the listener.
    NoBackend,
    /// Flow-table cap (`max_flows`) reached — shed the new flow, protect the
    /// existing ones.
    Shed,
    /// A backend datagram referenced an unknown / already-closed flow.
    UnknownFlow,
}

/// Metric events the core asks the shell to record. The shell owns the
/// `incr!`/`count!`/`gauge!`/`time!` macros (`lib/src/metrics/`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricEvent {
    /// A new flow was admitted (`udp.flows.created`, `udp.active_flows += 1`).
    FlowCreated,
    /// A flow was torn down by idle/teardown/drain (`udp.flows.evicted`,
    /// `udp.active_flows -= 1`).
    FlowEvicted,
    /// A new flow was shed at the cap (`udp.flows.shed`).
    FlowShed,
    /// A client→backend datagram was forwarded (`udp.datagrams.in`,
    /// `udp.bytes.in`). Carries the *payload* byte count (excludes any PPv2
    /// prefix the core adds).
    DatagramIn(usize),
    /// A backend→client datagram was returned (`udp.datagrams.out`,
    /// `udp.bytes.out`).
    DatagramOut(usize),
    /// A datagram was dropped, by reason (`udp.datagrams.dropped`).
    DatagramDropped(DropReason),
}

/// Per-cluster knobs the shell binds to a listener's flows. Mirrors the
/// `UdpClusterConfig` proto but stays in pure-core terms. Defaults match the
/// proto defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterConfig {
    /// Cluster the listener routes to.
    pub cluster: ClusterId,
    /// Key on `src_ip + src_port` (true) vs `src_ip` only (false). Maps the
    /// `UdpAffinityKey` proto enum.
    pub affinity_with_port: bool,
    /// Expected replies per flow before close. `0` = unlimited (DNS = 1).
    pub responses: u32,
    /// Max client datagrams per flow before close. `0` = unlimited.
    pub requests: u32,
    /// Idle timeout (client direction). Resets on every datagram.
    pub front_timeout: Duration,
    /// Idle timeout (upstream direction). Resets on every reply.
    pub back_timeout: Duration,
    /// Prepend a PPv2 DGRAM header to upstream datagrams.
    pub send_proxy_protocol: bool,
    /// PPv2 on every datagram (true) vs first-datagram-only (false, default).
    pub proxy_protocol_every_datagram: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            cluster: String::new(),
            affinity_with_port: false,
            responses: 0,
            requests: 0,
            front_timeout: Duration::from_secs(30),
            back_timeout: Duration::from_secs(30),
            send_proxy_protocol: false,
            proxy_protocol_every_datagram: false,
        }
    }
}

/// Control-plane events the shell feeds the manager via
/// [`ManagerInput::Config`]. Additive; an unknown event must never panic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigEvent {
    /// Replace the listener's cluster routing + per-cluster knobs. Applies to
    /// new flows; existing flows keep their captured config (stable affinity).
    SetCluster(ClusterConfig),
    /// Update the flow-table cap. Shrinking does not evict existing flows; it
    /// only sheds future ones.
    SetMaxFlows(usize),
    /// Update the maximum accepted rx datagram size.
    SetMaxRxDatagramSize(usize),
    /// Begin draining: admit no new flows; let existing ones reach teardown.
    Drain,
}

/// Logging prefix for the pure UDP core (tag `UDP`). Mirrors
/// `protocol/mux/mod.rs`'s `log_context!`. The core has no socket/ULID of its
/// own, so the manager-level prefix carries its admission counters; honours the
/// colored flag via [`ansi_palette`](sozu_command::logging::ansi_palette).
#[allow(unused_macros)]
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "[- - - -]\t{open}UDP{reset}\t{grey}Manager{reset}({gray}flows{reset}={white}{flows}{reset}, {gray}max_flows{reset}={white}{max_flows}{reset}, {gray}draining{reset}={white}{draining}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            flows = $self.flow_count(),
            max_flows = $self.max_flows(),
            draining = $self.is_draining(),
        )
    }};
}

/// Per-flow logging prefix (tag `UDP-FLOW`). Renders the canonical
/// `[session req cluster backend]`-style bracket reduced to the flow's stable
/// id + client + backend, so flow lines stay filterable.
#[allow(unused_macros)]
macro_rules! log_context_lite {
    ($flow_id:expr, $flow:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "[- - - -]\t{open}UDP-FLOW{reset}\t{grey}Flow{reset}({gray}id{reset}={white}{id}{reset}, {gray}client{reset}={white}{client}{reset}, {gray}backend{reset}={white}{backend:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            id = $flow_id,
            client = $flow.client,
            backend = $flow.backend_addr,
        )
    }};
}

#[allow(unused_imports)]
pub(crate) use {log_context, log_context_lite};
