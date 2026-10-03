//! HAProxy PROXY protocol v2 state surface plus the legacy v1 header model.
//!
//! Runtime expect, relay and send states use v2 only; v1 is not accepted on
//! the wire.
//!
//! Three distinct session roles compose into the front-end pipeline:
//! `expect` ingests an inbound v2 header and hands off to the downstream
//! protocol; `relay` forwards an existing v2 header verbatim onto a new
//! backend connection; `send` synthesises a v2 header before user bytes.
//! `header` and `parser` own the wire format. Long-form lifecycle:
//! `lib/src/protocol/proxy_protocol/LIFECYCLE.md` (created in this
//! changeset).

pub mod expect;
pub mod header;
pub mod parser;
pub mod relay;
pub mod send;
