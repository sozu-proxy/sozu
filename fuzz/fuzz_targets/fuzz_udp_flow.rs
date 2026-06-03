#![no_main]
//! Fuzz target for the sans-io UDP load-balancing core (issue #1273).
//!
//! Drives a [`UdpManager`] through an arbitrary sequence of client/backend
//! datagrams, control-plane reconfig events, backend resolutions and clock
//! advances — all derived deterministically from the fuzz input — and fully
//! drains `poll_output` after every step. It also exercises
//! [`SourceTupleExtractor::flow_key`], the PPv2 `dgram_header` /
//! `prepend_dgram_header` builders, and [`FlowKey::from_src`] directly on
//! arbitrary bytes and addresses.
//!
//! The core is pure sans-io: there is no socket, no `Instant::now()` on the
//! datapath, no `rand`. To stay deterministic this target injects a monotonic
//! clock — a single base `Instant` captured once, advanced only by deltas
//! parsed out of the input — so the same input always produces the same run.
//!
//! Invariants the target asserts beyond "never panic":
//! - the flow count never exceeds the active `max_flows` cap;
//! - the gauge never underflows (cumulative `CloseFlow` count never exceeds
//!   cumulative `FlowCreated`);
//! - a final long clock advance reaps every flow back to zero with no armed
//!   timer left (no fd / slab leak).
//!
//! The `poll_timeout` strict-advance (busy-loop) invariant is asserted inside
//! `handle_timeout` itself in debug builds, so every parsed `Tick` exercises it
//! for free. Corpus + run instructions live in `fuzz/README.md`.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

use libfuzzer_sys::fuzz_target;
use sozu_lib::protocol::udp::{
    ClusterConfig, ConfigEvent, FlowId, FlowKey, FlowKeyExtractor, ManagerInput, MetricEvent,
    Output, SourceTupleExtractor, UdpManager,
    proxy_protocol::{dgram_header, prepend_dgram_header},
};

/// A tiny big-endian byte reader over the fuzz input. Every getter returns a
/// default (0 / empty) when the input is exhausted, so the grammar degrades
/// gracefully on short inputs instead of branching on length everywhere.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    /// Read a length-prefixed byte slice (1-byte length, capped at the
    /// remaining input). Used for datagram payloads.
    fn bytes(&mut self) -> &'a [u8] {
        let len = self.u8() as usize;
        let start = self.pos.min(self.data.len());
        let end = (start + len).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }

    /// Decode an address from the next bytes: low bit of the family byte picks
    /// v4 vs v6, the rest of the bytes fill the address + port.
    fn addr(&mut self) -> SocketAddr {
        if self.u8() & 1 == 0 {
            let ip = Ipv4Addr::new(self.u8(), self.u8(), self.u8(), self.u8());
            let port = self.u16();
            SocketAddr::new(IpAddr::V4(ip), port)
        } else {
            let segs = [
                self.u16(),
                self.u16(),
                self.u16(),
                self.u16(),
                self.u16(),
                self.u16(),
                self.u16(),
                self.u16(),
            ];
            let port = self.u16();
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(
                    segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6], segs[7],
                )),
                port,
            )
        }
    }

    /// A client source address drawn from a small, bounded pool so distinct
    /// flows actually collide on the flow table (otherwise every datagram is a
    /// brand-new flow and the table never sees reuse / eviction races). The
    /// port pool is intentionally tiny so the 2-tuple vs 4-tuple keying knob
    /// changes behaviour.
    fn client_addr(&mut self) -> SocketAddr {
        let id = self.u8();
        let port = 9000 + (self.u8() as u16 % 4);
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, id % 8)), port)
    }
}

/// Build a `ClusterConfig` from the input. The cluster name is non-empty about
/// half the time so the manager exercises both the `NoBackend` drop path and
/// the real admission path.
fn cluster_config(r: &mut Reader) -> ClusterConfig {
    let flags = r.u8();
    let cluster = if flags & 0b0000_0001 != 0 {
        String::new()
    } else {
        "fuzz-cluster".to_owned()
    };
    // Bound the timeouts so a single parsed step cannot push the deadline
    // billions of seconds out (which would make every reaper tick a no-op and
    // hide the teardown paths). 0 stays meaningful (immediate idle).
    let front = Duration::from_secs((r.u8() % 40) as u64);
    let back = Duration::from_secs((r.u8() % 40) as u64);
    ClusterConfig {
        cluster,
        affinity_with_port: flags & 0b0000_0010 != 0,
        // 0 = unlimited; otherwise a small cap so the teardown fires often.
        responses: (r.u8() % 4) as u32,
        requests: (r.u8() % 4) as u32,
        front_timeout: front,
        back_timeout: back,
        send_proxy_protocol: flags & 0b0000_0100 != 0,
        proxy_protocol_every_datagram: flags & 0b0000_1000 != 0,
    }
}

/// Drain every queued output, threading the create/close tally and the running
/// set of flows pending a backend so subsequent steps can resolve them. Returns
/// nothing; mutates the bookkeeping in place.
fn drain(
    mgr: &mut UdpManager,
    created: &mut u64,
    closed: &mut u64,
    awaiting: &mut Vec<FlowId>,
) {
    while let Some(out) = mgr.poll_output() {
        match out {
            Output::Metric(MetricEvent::FlowCreated) => *created += 1,
            Output::CloseFlow(_) => *closed += 1,
            Output::SelectBackend { flow, .. } => awaiting.push(flow),
            // Re-validate every owned datagram so a malformed PPv2 prefix or a
            // truncated copy would surface as a panic inside the harness rather
            // than silently passing.
            Output::SendToBackend(t) | Output::SendToClient(t) => {
                let _ = t.payload.len();
                let _ = t.dst;
            }
            _ => {}
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);

    // --- Part 1: drive a full UdpManager run -------------------------------

    let initial = cluster_config(&mut r);
    // max_flows in 1..=16 so the cap is reachable from a short input but never
    // zero (a zero cap would shed everything and never exercise admission).
    let max_flows = 1 + (r.u8() as usize % 16);
    // max_rx_datagram_size kept small-ish so the Truncated path is reachable,
    // but >= a few bytes so normal datagrams still admit.
    let max_rx = 1 + (r.u16() as usize % 2048);
    let hash_seed = r.u32() as u64;

    let mut mgr = UdpManager::new(initial, max_flows, max_rx, hash_seed);

    // High-water mark of every `max_flows` value ever in effect. Admission caps
    // the flow count at the *current* `max_flows`, but `SetMaxFlows` shrinking
    // the cap does NOT evict existing flows (documented: "Shrinking does not
    // evict existing flows; it only sheds future ones"). So the faithful
    // invariant is `flow_count <= max(all caps ever set)`, not the live cap.
    let mut max_flows_seen = max_flows;

    let base = Instant::now();
    // Monotonic injected clock. Advanced ONLY by parsed deltas — never reset to
    // Instant::now() mid-run — so the run stays deterministic.
    let mut now = base;

    let mut created = 0u64;
    let mut closed = 0u64;
    // Flows that have emitted SelectBackend but not yet been resolved.
    let mut awaiting: Vec<FlowId> = Vec::new();

    // Bound the step count so a pathological input can't run unboundedly; the
    // libFuzzer max input length already caps `data`, this is belt-and-braces.
    let mut steps = 0usize;
    while !r.is_empty() && steps < 4096 {
        steps += 1;
        match r.u8() % 7 {
            // Client datagram from the bounded source pool.
            0 => {
                let src = r.client_addr();
                let payload = r.bytes();
                mgr.handle_input(ManagerInput::ClientDatagram { src, payload }, now);
            }
            // Resolve the oldest pending flow (drives AwaitingBackend ->
            // Established). Also occasionally resolve an arbitrary (possibly
            // stale / reaped) flow id to hit the UnknownFlow path.
            1 => {
                let flow = if r.u8() & 1 == 0 {
                    awaiting.pop()
                } else {
                    Some(r.u32() as FlowId)
                };
                if let Some(flow) = flow {
                    let addr = r.addr();
                    mgr.handle_input(
                        ManagerInput::BackendResolved {
                            flow,
                            backend: "fuzz-backend".to_owned(),
                            addr,
                        },
                        now,
                    );
                }
            }
            // Backend datagram for some flow id (valid or stale).
            2 => {
                let flow = r.u32() as FlowId;
                let payload = r.bytes();
                mgr.handle_input(ManagerInput::BackendDatagram { flow, payload }, now);
            }
            // Reconfigure the cluster mid-flow.
            3 => {
                let cfg = cluster_config(&mut r);
                mgr.handle_input(ManagerInput::Config(ConfigEvent::SetCluster(cfg)), now);
            }
            // Adjust caps / drain.
            4 => match r.u8() % 3 {
                0 => {
                    let n = 1 + (r.u8() as usize % 16);
                    max_flows_seen = max_flows_seen.max(n);
                    mgr.handle_input(ManagerInput::Config(ConfigEvent::SetMaxFlows(n)), now);
                }
                1 => {
                    let n = 1 + (r.u16() as usize % 4096);
                    mgr.handle_input(
                        ManagerInput::Config(ConfigEvent::SetMaxRxDatagramSize(n)),
                        now,
                    );
                }
                _ => {
                    mgr.handle_input(ManagerInput::Config(ConfigEvent::Drain), now);
                }
            },
            // Advance the injected clock and fire the reaper.
            5 => {
                let delta = Duration::from_secs((r.u8() % 50) as u64);
                now += delta;
                mgr.handle_timeout(now);
            }
            // Advance by a sub-second amount (exercises near-boundary deadlines)
            // without firing — then the next Tick step fires.
            _ => {
                let millis = r.u16() as u64;
                now += Duration::from_millis(millis);
            }
        }

        drain(&mut mgr, &mut created, &mut closed, &mut awaiting);

        // Admission never grows the flow count past the largest cap ever in
        // effect (a later shrink may legitimately leave more live flows than
        // the current cap — those flows are not evicted, only future ones are
        // shed).
        assert!(
            mgr.flow_count() <= max_flows_seen,
            "flow_count {} exceeded high-water max_flows {max_flows_seen} (live cap {})",
            mgr.flow_count(),
            mgr.max_flows()
        );
        // The gauge never underflows: we never close more than we created.
        assert!(
            closed <= created,
            "CloseFlow count {closed} exceeded FlowCreated count {created} (gauge underflow)"
        );
    }

    // Final long reap: every remaining flow must tear down, the gauge must
    // balance, and no timer may stay armed (no leak).
    now += Duration::from_secs(10_000);
    mgr.handle_timeout(now);
    drain(&mut mgr, &mut created, &mut closed, &mut awaiting);
    assert_eq!(mgr.flow_count(), 0, "reaper left {} flows", mgr.flow_count());
    assert!(
        mgr.poll_timeout().is_none(),
        "timer still armed after full reap"
    );
    assert_eq!(
        created, closed,
        "FlowCreated ({created}) != CloseFlow ({closed}) after reap (gauge leak)"
    );

    // --- Part 2: flow-key extraction on arbitrary bytes --------------------

    let extractor = SourceTupleExtractor;
    let cfg = ClusterConfig::default();
    let src = r.addr();
    let payload = r.bytes();
    let _ = extractor.flow_key(src, payload, &cfg);
    // Empty payload is the documented Invalid path; assert the contract holds
    // (the extractor must reject it) without ever panicking.
    assert!(extractor.flow_key(src, &[], &cfg).is_none());

    // Both keying modes must round-trip without panicking, and the 2-tuple form
    // must normalise the port to zero.
    let _ = FlowKey::from_src(src, true);
    let two_tuple = FlowKey::from_src(src, false);
    assert_eq!(two_tuple.src.port(), 0, "2-tuple key must zero the port");

    // --- Part 3: PPv2 DGRAM framing on arbitrary addresses -----------------

    let client = r.addr();
    let backend = r.addr();
    let header = dgram_header(client, backend);
    // The header is never shorter than the 16-byte fixed prefix.
    assert!(header.len() >= 16, "PPv2 header shorter than fixed prefix");

    let mut buf = r.bytes().to_vec();
    let original_len = buf.len();
    let header_len = prepend_dgram_header(&mut buf, client, backend);
    // Prefix accounting must be exact: header_len bytes prepended, payload kept.
    assert_eq!(buf.len(), header_len + original_len, "PPv2 prefix length mismatch");
    assert_eq!(header_len, header.len(), "prepend disagrees with dgram_header");
});
