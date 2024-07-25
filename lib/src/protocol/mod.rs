pub mod h2;
pub mod kawa_h1;
pub mod mux;
pub mod pipe;
pub mod proxy_protocol;
pub mod rustls;

use std::{cell::RefCell, rc::Rc};

use mio::Token;
use sozu_command::ready::Ready;

use crate::{
    L7Proxy, ProxySession, SessionIsToBeClosed, SessionMetrics, SessionResult, StateResult,
};

pub use crate::protocol::{
    http::Http, kawa_h1 as http, pipe::Pipe, proxy_protocol::send::SendProxyProtocol,
    rustls::TlsHandshake,
};

/// All States should satisfy this trait in order to receive and handle Session events
pub trait SessionState {
    /// if a session received an event or can still execute, the event loop will
    /// call this method. Its result indicates if it can still execute or if the
    /// session can be closed
    fn ready<P: L7Proxy>(
        &mut self,
        session: Rc<RefCell<dyn ProxySession>>,
        proxy: Rc<RefCell<P>>,
        metrics: &mut SessionMetrics,
    ) -> SessionResult;
    /// if the event loop got an event for a token associated with the session,
    /// it will call this method
    fn update_readiness(&mut self, token: Token, events: Ready);
    /// close the state
    fn close<P: L7Proxy>(&mut self, _proxy: Rc<RefCell<P>>, _metrics: &mut SessionMetrics) {}
    /// if a timeout associated with the session triggers, the event loop will
    /// call this method with the timeout's token
    fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> StateResult;
    /// cancel frontend timeout (and backend timeout if present)
    fn cancel_timeouts(&mut self);
    /// display the session's internal state (for debugging purpose),
    /// ```plain
    /// <context> Session(<State name>):
    ///     Frontend:
    ///         - Token(...) Readiness(...)
    ///     Backends:
    ///         - Token(...) Readiness(...)
    ///         - Token(...) Readiness(...)
    /// ```
    fn print_state(&self, context: &str);
    /// tell the session it has to shut down if possible
    ///
    /// if the session handles HTTP requests, it will not close until the response
    /// is completely sent back to the client
    fn shutting_down(&mut self) -> SessionIsToBeClosed {
        true
    }
}
