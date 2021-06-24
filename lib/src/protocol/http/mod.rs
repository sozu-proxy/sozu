use std::cmp::min;
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::net::{SocketAddr,IpAddr};
use mio::*;
use mio::net::TcpStream;
use rusty_ulid::Ulid;
use time::{Instant, Duration};
use super::super::{SessionResult,Protocol,Readiness,SessionMetrics, LogDuration};
use buffer_queue::BufferQueue;
use socket::{SocketHandler, SocketResult, TransportProtocol};
use protocol::ProtocolResult;
use pool::Pool;
use util::UnwrapLog;
use timer::TimeoutContainer;
use sozu_command::ready::Ready;
use crate::Backend;

pub mod parser;
pub mod cookies;
pub mod answers;

use self::parser::{parse_request_until_stop, parse_response_until_stop,
  RequestState, ResponseState, Chunk, Continue, RRequestLine, RStatusLine,
  Method, compare_no_case};

#[derive(Clone)]
pub struct StickySession {
  pub sticky_id: String
}

impl StickySession {
  pub fn new(backend_id: String) -> StickySession {
    StickySession {
      sticky_id: backend_id
    }
  }
}

#[derive(Debug,Clone,PartialEq)]
pub enum SessionStatus {
  Normal,
  /// status, HTTP answer, index in HTTP answer
  DefaultAnswer(DefaultAnswerStatus, Rc<Vec<u8>>, usize),
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum DefaultAnswerStatus {
  Answer301,
  Answer400,
  Answer404,
  Answer408,
  Answer413,
  Answer502,
  Answer503,
  Answer504,
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum TimeoutStatus {
  Request,
  Response,
  WaitingForNewRequest,
  WaitingForResponse,
}

pub struct Http<Front:SocketHandler> {
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  frontend_token:     Token,
  backend_token:      Option<Token>,
  pub status:         SessionStatus,
  pub front_buf:      Option<BufferQueue>,
  pub back_buf:       Option<BufferQueue>,
  pub app_id:         Option<String>,
  pub request_id:     Ulid,
  pub backend_id:     Option<String>,
  pub front_readiness:Readiness,
  pub back_readiness: Readiness,
  pub public_address: SocketAddr,
  pub session_address: Option<SocketAddr>,
  pub backend_data:   Option<Rc<RefCell<Backend>>>,
  pub sticky_name:    String,
  pub sticky_session: Option<StickySession>,
  pub protocol:       Protocol,
  pub request:        Option<RequestState>,
  pub response:       Option<ResponseState>,
  pub req_header_end: Option<usize>,
  pub res_header_end: Option<usize>,
  pub added_req_header: Option<AddedRequestHeader>,
  pub added_res_header: String,
  pub keepalive_count: usize,
  pub backend_stop:    Option<Instant>,
  answers:             Rc<RefCell<answers::HttpAnswers>>,
  pub closing:         bool,
  pool:                Weak<RefCell<Pool>>,
  pub front_timeout:   TimeoutContainer,
  pub back_timeout:    TimeoutContainer,
  pub frontend_timeout_duration: Duration,
}

impl<Front:SocketHandler> Http<Front> {
  pub fn new(sock: Front, token: Token, request_id: Ulid, pool: Weak<RefCell<Pool>>,
    public_address: SocketAddr, session_address: Option<SocketAddr>, sticky_name: String,
    protocol: Protocol, answers: Rc<RefCell<answers::HttpAnswers>>,
    front_timeout: TimeoutContainer,
    frontend_timeout_duration: Duration, backend_timeout_duration: Duration) -> Http<Front> {

    let mut session = Http {
      frontend:           sock,
      backend:            None,
      frontend_token:     token,
      backend_token:      None,
      status:             SessionStatus::Normal,
      front_buf:          None,
      back_buf:           None,
      app_id:             None,
      request_id,
      backend_id:         None,
      front_readiness:    Readiness::new(),
      back_readiness:     Readiness::new(),
      public_address,
      session_address,
      backend_data: None,
      sticky_name,
      sticky_session:     None,
      protocol,
      request:        Some(RequestState::Initial),
      response:       Some(ResponseState::Initial),
      req_header_end: None,
      res_header_end: None,
      added_req_header: None,
      added_res_header: String::from(""),
      keepalive_count: 0,
      backend_stop:    None,
      closing:         false,
      front_timeout,
      back_timeout: TimeoutContainer::new_empty(backend_timeout_duration),
      frontend_timeout_duration,
      answers,
      pool,
    };
    session.added_req_header = Some(session.added_request_header(session_address));
    session.added_res_header = session.added_response_header();

    session
  }

  pub fn reset(&mut self) {
    let request_id = Ulid::generate();
    //info!("{} RESET TO {}", self.log_ctx, request_id);
    gauge_add!("http.active_requests", -1);

    self.request = Some(RequestState::Initial);
    self.response = Some(ResponseState::Initial);
    self.req_header_end = None;
    self.res_header_end = None;
    self.added_req_header = Some(self.added_request_header(self.session_address));
    self.added_res_header = self.added_response_header();

    // if HTTP requests are pipelined, we might still have some data in the front buffer
    if self.front_buf.as_ref().map(|buf| !buf.empty()).unwrap_or(false) {
      self.front_readiness.event.insert(Ready::readable());
    } else {
      self.front_buf = None;
    }

    self.back_buf = None;
    self.request_id = request_id;
    self.keepalive_count += 1;

    if let Some(ref mut b) = self.backend_data {
      let mut backend = b.borrow_mut();
      backend.active_requests = backend.active_requests.saturating_sub(1);
    }

    // reset the front timeout and cancel the back timeout while we are
    // waiting for a new request
    self.front_timeout.reset();
    self.back_timeout.cancel();
  }

  pub fn log_context(&self) -> LogContext {
    LogContext {
      request_id: self.request_id,
      app_id: self.app_id.as_deref(),
      backend_id: self.backend_id.as_deref(),
    }
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(back) = self.backend_token {
      return Some((self.frontend_token, back))
    }
    None
  }

  pub fn print_state(&self, prefix: &str) -> String {
    format!("{}: request: {:?}, request header end: {:?}, response: {:?}, response header end: {:?}",
      prefix, self.request, self.req_header_end, self.response, self.res_header_end)
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Option<Rc<Vec<u8>>>)  {
    self.front_buf = None;
    self.back_buf = None;

    if let SessionStatus::DefaultAnswer(status, _, _) = self.status {
      error!("already set the default answer to {:?}, trying to set to {:?}", status, answer);
    } else {
      match answer {
        DefaultAnswerStatus::Answer301 => incr!("http.301.redirection"),
        DefaultAnswerStatus::Answer400 => incr!("http.400.errors"),
        DefaultAnswerStatus::Answer404 => incr!("http.404.errors"),
        DefaultAnswerStatus::Answer408 => incr!("http.408.errors"),
        DefaultAnswerStatus::Answer413 => incr!("http.413.errors"),
        DefaultAnswerStatus::Answer502 => incr!("http.502.errors"),
        DefaultAnswerStatus::Answer503 => incr!("http.503.errors"),
        DefaultAnswerStatus::Answer504 => incr!("http.504.errors"),
      };
    }

    let buf = buf.unwrap_or_else(|| self.answers.borrow().get(answer, self.app_id.as_deref()));
    self.status = SessionStatus::DefaultAnswer(answer, buf, 0);
    self.front_readiness.interest = Ready::writable() | Ready::hup() | Ready::error();
    self.back_readiness.interest  = Ready::hup() | Ready::error();

  }

  fn added_request_header(&self, client_address: Option<SocketAddr>) -> AddedRequestHeader {
      AddedRequestHeader {
          request_id: self.request_id,
          closing: self.closing,
          public_address: self.public_address,
          peer_address: client_address.or_else(|| self.front_socket().peer_addr().ok()),
          protocol: self.protocol,
      }
  }

  pub fn added_response_header(&self) -> String {
    if self.closing {
      format!("Sozu-Id: {}\r\nConnection: close", self.request_id)
    } else {
      format!("Sozu-Id: {}\r\n", self.request_id)
    }
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn front_socket_mut(&mut self) -> &mut TcpStream {
    self.frontend.socket_mut()
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn back_socket_mut(&mut self)  -> Option<&mut TcpStream> {
    self.backend.as_mut()
  }

  pub fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  pub fn test_back_socket(&mut self) -> bool {
    match self.backend {
      Some(ref mut s) => {
        let mut tmp = [0u8; 1];
        let res = s.peek(&mut tmp[..]);

        match res {
          // if the socket is half open, it will report 0 bytes read (EOF)
          Ok(0) => false,
          Ok(_) => true,
          Err(e) => match e.kind() {
             std::io::ErrorKind::WouldBlock => true,
             _ => false,
          }
        }
      },
      None => {
        false
      }
    }
  }

  pub fn is_valid_backend_socket(&mut self) -> bool {
    // if socket was not used in the last second, test it
    if self.backend_stop.as_ref().map(|t| {
      let now = Instant::now();
      let dur = now - *t;

      dur > Duration::seconds(1)
    }).unwrap_or(true) {
      return self.test_back_socket();
    }

    return true;
  }

  pub fn close(&mut self) {
  }

  pub fn set_back_socket(&mut self, socket: TcpStream, backend: Option<Rc<RefCell<Backend>>>) {
    self.backend = Some(socket);
    self.backend_data = backend;
  }

  pub fn set_app_id(&mut self, app_id: String) {
    self.app_id = Some(app_id);
  }

  pub fn set_backend_id(&mut self, backend_id: String) {
    self.backend_id = Some(backend_id);
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn clear_back_token(&mut self) {
    self.backend_token = None;
  }

  pub fn front_readiness(&mut self) -> &mut Readiness {
    &mut self.front_readiness
  }

  pub fn back_readiness(&mut self) -> &mut Readiness {
    &mut self.back_readiness
  }

  pub fn set_back_timeout(&mut self, dur: Duration) {
      if let Some(token) = self.backend_token.as_ref() {
          self.back_timeout.set_duration(dur);
          self.back_timeout.set(*token);
      }
  }

  fn protocol(&self) -> Protocol {
    self.protocol
  }

  fn must_continue_request(&self) -> bool {
    if let Some(Continue::Expects(_sz)) = self.request.as_ref().and_then(|r| r.get_keep_alive().as_ref().map(|conn| conn.continues)) {
      true
    } else {
      false
    }
  }

  fn must_continue_response(&self) -> Option<usize> {
    if let Some(Continue::Expects(sz)) = self.request.as_ref().and_then(|r| r.get_keep_alive().map(|conn| conn.continues)) {
      if self.response.as_ref().and_then(|r| r.get_status_line().map(|st| st.status == 100)).unwrap_or(false) {
        return Some(sz);
      }
    }
    None
  }

  pub fn timeout_status(&self) -> TimeoutStatus {
    match self.request.as_ref() {
      Some(RequestState::Request(_,_,_)) | Some(RequestState::RequestWithBody(_,_,_,_)) |
        Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
          match self.response.as_ref() {
            Some(ResponseState::Initial) => TimeoutStatus::WaitingForResponse,
            _ => TimeoutStatus::Response,
          }
      },
      _ => if self.keepalive_count > 0 {
        TimeoutStatus::WaitingForNewRequest
      } else {
        TimeoutStatus::Request
      },
    }
  }

  pub fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.log_context(), self.frontend_token.0,
      self.backend_token.map(|t| format!("{}", t.0)).unwrap_or_else(|| "-".to_string()));
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.cancel_backend_timeout();
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  pub fn front_hup(&mut self) -> SessionResult {
    SessionResult::CloseSession
  }

  pub fn back_hup(&mut self) -> SessionResult {
    if let Some(ref mut buf) = self.back_buf {
      //FIXME: closing the session might not be a good idea if we do keep alive on the front here?
      if buf.output_data_size() == 0 || buf.next_output_data().is_empty() {
        if self.back_readiness.event.is_readable() {
          self.back_readiness.interest.insert(Ready::readable());
          SessionResult::Continue
        } else {
          self.set_answer(DefaultAnswerStatus::Answer502, None);
          // we're not expecting any more data from the backend
          self.back_readiness.interest  = Ready::empty();
          SessionResult::Continue
        }
      } else {
        self.front_readiness.interest.insert(Ready::writable());
        if self.back_readiness.event.is_readable() {
          self.back_readiness.interest.insert(Ready::readable());
        }
        SessionResult::Continue
      }
    } else {
      SessionResult::CloseSession
    }
  }

  pub fn shutting_down(&mut self) -> SessionResult {
    if self.request.as_ref().map(|r| *r == RequestState::Initial).unwrap_or(false)
      && self.front_buf.as_ref().map(|b| !b.empty()).unwrap_or(false)
      && self.back_buf.as_ref().map(|b| !b.empty()).unwrap_or(false) {
        SessionResult::CloseSession
    } else {
      self.closing = true;
      SessionResult::Continue
    }
  }

  /// Retrieve the response status from the http response state
  pub fn get_response_status(&self) -> Option<&RStatusLine> {
    self.response.as_ref().and_then(|r| r.get_status_line())
  }

  pub fn get_host(&self) -> Option<&str> {
    self.request.as_ref().and_then(|r| r.get_host())
  }

  pub fn get_request_line(&self) -> Option<&RRequestLine> {
    self.request.as_ref().and_then(|r| r.get_request_line())
  }

  pub fn get_session_address(&self) -> Option<SocketAddr> {
    self.session_address.or_else(|| self.frontend.socket_ref().peer_addr().ok())
  }

  pub fn get_backend_address(&self) -> Option<SocketAddr> {
    self.backend_data.as_ref().map(|b| b.borrow().address)
        .or_else( || self.backend.as_ref().and_then(|backend| backend.peer_addr().ok()))
  }

  pub fn websocket_context(&self) -> String {
    let host         = self.get_host().unwrap_or("-");
    let request_line = self.get_request_line().map(|line| (&line.method, line.uri.as_str())).unwrap_or((&Method::Get, "-"));
    let status_line  = self.get_response_status().map(|line| (line.status, line.reason.as_str())).unwrap_or((0, "-"));

    format!("{} {} {}\t{} {}", host, request_line.0, request_line.1, status_line.0, status_line.1)
  }


  fn protocol_string(&self) -> &'static str {
    match self.protocol() {
      Protocol::HTTP  => "HTTP",
      Protocol::HTTPS => {
        match self.frontend.protocol() {
          TransportProtocol::Ssl2   => "HTTPS-SSL2",
          TransportProtocol::Ssl3   => "HTTPS-SSL3",
          TransportProtocol::Tls1_0 => "HTTPS-TLS1.0",
          TransportProtocol::Tls1_1 => "HTTPS-TLS1.1",
          TransportProtocol::Tls1_2 => "HTTPS-TLS1.2",
          TransportProtocol::Tls1_3 => "HTTPS-TLS1.3",
          _                         => unreachable!()
        }
      }
      _ => unreachable!()
    }
  }

  pub fn log_request_success(&self, metrics: &SessionMetrics) {
    let session = SessionAddress(self.get_session_address());
    let backend = SessionAddress(self.get_backend_address());

    let host         = OptionalString::new(self.get_host());
    let request_line = OptionalRequest::new(self.get_request_line().map(|line| (&line.method, line.uri.as_str())));
    let status_line  = OptionalStatus::new(self.get_response_status().map(|line| (line.status, line.reason.as_str())));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();
    let _wait_time  = metrics.wait_time;

    let app_id = OptionalString::new(self.app_id.as_ref().map(|s| s.as_str()));
    time!("response_time", app_id.as_str(), response_time.whole_milliseconds());
    time!("service_time", app_id.as_str(), service_time.whole_milliseconds());
    time!("response_time", response_time.whole_milliseconds());
    time!("service_time", service_time.whole_milliseconds());

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.whole_milliseconds(),
          metrics.backend_connection_time(), metrics.backend_bin, metrics.backend_bout);
      }
    }

    let proto = self.protocol_string();

    info_access!("{}{} -> {}\t{} {} {} {}\t{} {} {}\t{}",
      self.log_context(), session, backend,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      proto, host, request_line, status_line);
  }

  pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
    let session = SessionAddress(self.get_session_address());
    let backend = SessionAddress(self.get_backend_address());

    let status_line = match self.status {
      SessionStatus::Normal => "-",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer301, _, _) => "301 Moved Permanently",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer400, _, _) => "400 Bad Request",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer404, _, _) => "404 Not Found",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer408, _, _) => "408 Request Timeout",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer413, _, _) => "413 Payload Too Large",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer502, _, _) => "502 Bad Gateway",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer503, _, _) => "503 Service Unavailable",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer504, _, _) => "504 Gateway Timeout",
    };

    let host         = OptionalString::new(self.get_host());
    let request_line = OptionalRequest::new(self.get_request_line().map(|line| (&line.method, line.uri.as_str())));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    if let Some(ref app_id) = self.app_id {
      time!("response_time", &app_id, response_time.whole_milliseconds());
      time!("service_time", &app_id, service_time.whole_milliseconds());
    }
    time!("response_time", response_time.whole_milliseconds());
    time!("service_time", service_time.whole_milliseconds());
    incr!("http.errors");

    let proto = self.protocol_string();

    info_access!("{}{} -> {}\t{} {} {} {}\t{} {} {}\t{}",
      self.log_context(), session, backend,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      proto, host, request_line, status_line);
  }

  pub fn log_request_error(&mut self, metrics: &mut SessionMetrics, message: &str) {
    metrics.service_stop();
    self.front_readiness.reset();
    self.back_readiness.reset();

    let session = SessionAddress(self.get_session_address());
    let backend = SessionAddress(self.get_backend_address());

    let host         = OptionalString::new(self.get_host());
    let request_line = OptionalRequest::new(self.get_request_line().map(|line| (&line.method, line.uri.as_str())));
    let status_line  = OptionalStatus::new(self.get_response_status().as_ref().map(|line| (line.status, line.reason.as_str())));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    if let Some(ref app_id) = self.app_id {
      time!("response_time", &app_id, response_time.whole_milliseconds());
      time!("service_time", &app_id, service_time.whole_milliseconds());
    }
    time!("response_time", response_time.whole_milliseconds());
    time!("service_time", service_time.whole_milliseconds());
    incr!("http.errors");
    /*
    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    time!("request_time", &app_id, response_time);

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.whole_milliseconds(), metrics.backend_connection_time(), metrics.backend_bin, metrics.backend_bout);
      }
    }*/

    let proto = self.protocol_string();

    error_access!("{}{} -> {}\t{} {} {} {}\t{} {} {}\t{} | {}",
      self.log_context(), session, backend,
      LogDuration(response_time), LogDuration(service_time), metrics.bin, metrics.bout,
      proto, host, request_line, status_line, message);
  }

  // Read content from the session
  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    if !self.front_timeout.reset() {
        //error!("could not reset front timeout");
    }

    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      self.front_readiness.interest.insert(Ready::writable());
      self.back_readiness.interest.remove(Ready::readable());
      self.back_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    assert!(!unwrap_msg!(self.request.as_ref()).is_front_error());

    if self.front_buf.is_none() {
      if let Some(p) = self.pool.upgrade() {
        if let Some(buf) = p.borrow_mut().checkout() {
          self.front_buf = Some(BufferQueue::with_buffer(buf));
        } else {
          error!("cannot get front buffer from pool, closing");
          return SessionResult::CloseSession;
        }
      }
    }

    if self.front_buf.as_ref().unwrap().buffer.available_space() == 0 {
      if self.backend_token == None {
        self.set_answer(DefaultAnswerStatus::Answer413, None);
        self.front_readiness.interest.remove(Ready::readable());
        self.front_readiness.interest.insert(Ready::writable());
      } else {
        self.front_readiness.interest.remove(Ready::readable());
        self.back_readiness.interest.insert(Ready::writable());
      }
      return SessionResult::Continue;
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.as_mut().unwrap().buffer.space());
    debug!("{}\tFRONT: read {} bytes", self.log_context(), sz);

    if sz > 0 {
      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      self.front_buf.as_mut().map(|front_buf| {
        front_buf.buffer.fill(sz);
        front_buf.sliced_input(sz);
        if front_buf.start_parsing_position > front_buf.parsed_position {
          let to_consume = min(front_buf.input_data_size(), front_buf.start_parsing_position - front_buf.parsed_position);
          front_buf.consume_parsed_data(to_consume);
        }
      });

      if self.front_buf.as_ref().unwrap().buffer.available_space() == 0 {
        self.front_readiness.interest.remove(Ready::readable());
      }
    } else {
      self.front_readiness.event.remove(Ready::readable());
    }

    match res {
      SocketResult::Error => {
        //we were in keep alive but the peer closed the connection
        if self.request == Some(RequestState::Initial) {
          // count an error if we were waiting for the first request
          // otherwise, if we already had one completed request and response,
          // and are waiting for the next one, we do not count a socket
          // closing abruptly as an error
          if self.keepalive_count == 0 {
              self.frontend.read_error();
          }

          metrics.service_stop();
          self.front_readiness.reset();
          self.back_readiness.reset();
        } else {
          self.frontend.read_error();
          let front_readiness = self.front_readiness.clone();
          let back_readiness  = self.back_readiness.clone();
          self.log_request_error(metrics,
            &format!("front socket error, closing the session. Readiness: {:?} -> {:?}, read {} bytes",
              front_readiness, back_readiness, sz));
        }
        return SessionResult::CloseSession;
      },
      SocketResult::Closed => {
        //we were in keep alive but the peer closed the connection
        if self.request == Some(RequestState::Initial) {
          // count an error if we were waiting for the first request
          // otherwise, if we already had one completed request and response,
          // and are waiting for the next one, we do not count a socket
          // closing abruptly as an error
          if self.keepalive_count == 0 {
              self.frontend.read_error();
          }

          metrics.service_stop();
          self.front_readiness.reset();
          self.back_readiness.reset();
        } else {
          self.frontend.read_error();
          let front_readiness = self.front_readiness.clone();
          let back_readiness  = self.back_readiness.clone();
          self.log_request_error(metrics,
            &format!("front socket was closed, closing the session. Readiness: {:?} -> {:?}, read {} bytes",
              front_readiness, back_readiness, sz));
        }
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.front_readiness.event.remove(Ready::readable());
      },
      SocketResult::Continue => {}
    };

    self.readable_parse(metrics)
  }


  pub fn readable_parse(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    let is_initial = self.request == Some(RequestState::Initial);
    // if there's no host, continue parsing until we find it
    let has_host = self.request.as_ref().map(|r| r.has_host()).unwrap_or(false);
    if !has_host {
      let (request_state, header_end) = (self.request.take().unwrap(), self.req_header_end.take());
      let (request_state, header_end) = parse_request_until_stop(request_state,
        header_end, &mut self.front_buf.as_mut().unwrap(),
        self.added_req_header.as_ref(),
        &self.sticky_name);

      self.request = Some(request_state);
      self.req_header_end = header_end;

      if unwrap_msg!(self.request.as_ref()).is_front_error() {
        incr!("http.front_parse_errors");

        self.set_answer(DefaultAnswerStatus::Answer400, None);
        gauge_add!("http.active_requests", 1);

        return SessionResult::Continue;
      }

      let is_now_initial = self.request == Some(RequestState::Initial);
      if is_initial && !is_now_initial {
        gauge_add!("http.active_requests", 1);
        incr!("http.requests");
      }

      if unwrap_msg!(self.request.as_ref()).has_host() {
        self.back_readiness.interest.insert(Ready::writable());
        return SessionResult::ConnectBackend;
      } else {
        self.front_readiness.interest.insert(Ready::readable());
        return SessionResult::Continue;
      }
    }

    self.back_readiness.interest.insert(Ready::writable());
    match self.request {
      Some(RequestState::Request(_,_,_)) | Some(RequestState::RequestWithBody(_,_,_,_)) => {
        if ! self.front_buf.as_ref().unwrap().needs_input() {
          // stop reading
          self.front_readiness.interest.remove(Ready::readable());
        }

        // if it was the first request, the front timeout duration
        // was set to request_timeout, which is much lower. For future
        // requests on this connection, we can wait a bit more
        self.front_timeout.set_duration(self.frontend_timeout_duration);

        SessionResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
        error!("{}\tfront read should have stopped on chunk ended", self.log_context());
        self.front_readiness.interest.remove(Ready::readable());
        SessionResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Error)) => {
        self.log_request_error(metrics, "front read should have stopped on chunk error");
        SessionResult::CloseSession
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
        // if it was the first request, the front timeout duration
        // was set to request_timeout, which is much lower. For future
        // requests on this connection, we can wait a bit more
        self.front_timeout.set_duration(self.frontend_timeout_duration);

        if ! self.front_buf.as_ref().unwrap().needs_input() {
          let (request_state, header_end) = (self.request.take().unwrap(), self.req_header_end.take());
          let (request_state, header_end) = parse_request_until_stop(request_state,
            header_end, &mut self.front_buf.as_mut().unwrap(),
            self.added_req_header.as_ref(),
            &self.sticky_name);

          self.request = Some(request_state);
          self.req_header_end = header_end;

          if unwrap_msg!(self.request.as_ref()).is_front_error() {
            self.log_request_error(metrics, "front chunk parsing error, closing the connection");
            return SessionResult::CloseSession;
          }

          if let Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) = self.request {
            self.front_readiness.interest.remove(Ready::readable());
          }
        }
        self.back_readiness.interest.insert(Ready::writable());
        SessionResult::Continue
      },
    _ => {
        let (request_state, header_end) = (self.request.take().unwrap(), self.req_header_end.take());
        let (request_state, header_end) = parse_request_until_stop(request_state,
          header_end, &mut self.front_buf.as_mut().unwrap(),
          self.added_req_header.as_ref(),
          &self.sticky_name);

        self.request = Some(request_state);
        self.req_header_end = header_end;

        if unwrap_msg!(self.request.as_ref()).is_front_error() {
          self.set_answer(DefaultAnswerStatus::Answer400, None);
          return SessionResult::Continue;
        }

        if let Some(RequestState::Request(_,_,_)) = self.request {
          self.front_readiness.interest.remove(Ready::readable());
        }
        self.back_readiness.interest.insert(Ready::writable());
        SessionResult::Continue
      }
    }
  }

  fn writable_default_answer(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    let res = if let SessionStatus::DefaultAnswer(_, ref buf, mut index) = self.status {
      let len = buf.len();

      let mut sz = 0usize;
      let mut res = SocketResult::Continue;
      while res == SocketResult::Continue && index < len {
        let (current_sz, current_res) = self.frontend.socket_write(&buf[index..]);
        res = current_res;
        sz += current_sz;
        index += current_sz;
      }

      count!("bytes_out", sz as i64);
      metrics.bout += sz;

      if res != SocketResult::Continue {
        self.front_readiness.event.remove(Ready::writable());
      }

      if index == len {
        metrics.service_stop();
        self.log_default_answer_success(&metrics);
        self.front_readiness.reset();
        self.back_readiness.reset();
        return SessionResult::CloseSession;
      }

      res
    } else {
      return SessionResult::CloseSession;
    };

    if res == SocketResult::Error {
      self.frontend.write_error();
      self.log_request_error(metrics, "error writing default answer to front socket, closing");
      SessionResult::CloseSession
    } else {
      SessionResult::Continue
    }
  }

  // Forward content to session
  pub fn writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    //handle default answers
    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      return self.writable_default_answer(metrics);
    }

    if self.back_buf.is_none() {
      error!("no back buffer to write on the front socket");
      return SessionResult::CloseSession;
    }

    let output_size = self.back_buf.as_ref().unwrap().output_data_size();
    if self.back_buf.as_ref().map(|buf| buf.output_data_size() == 0 || buf.next_output_data().is_empty()).unwrap() {
      self.back_readiness.interest.insert(Ready::readable());
      self.front_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    let mut sz = 0usize;
    let mut res = SocketResult::Continue;
    while res == SocketResult::Continue && self.back_buf.as_ref().unwrap().output_data_size() > 0 {
      // no more data in buffer, stop here
      if self.back_buf.as_ref().unwrap().next_output_data().is_empty() {
        self.back_readiness.interest.insert(Ready::readable());
        self.front_readiness.interest.remove(Ready::writable());
        count!("bytes_out", sz as i64);
        metrics.bout += sz;
        return SessionResult::Continue;
      }
      //let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.as_ref().unwrap().next_output_data());
      let (current_sz, current_res) = if self.frontend.has_vectored_writes() {
        let bufs = self.back_buf.as_ref().unwrap().as_ioslice();
        if bufs.is_empty() {
          break;
        }
        self.frontend.socket_write_vectored(&bufs)
      } else {
        self.frontend.socket_write(self.back_buf.as_ref().unwrap().next_output_data())
      };

      res = current_res;
      self.back_buf.as_mut().unwrap().consume_output_data(current_sz);
      sz += current_sz;
    }
    count!("bytes_out", sz as i64);
    metrics.bout += sz;

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}, buffer position {} restart position {}", self.log_context(), front.0, back.0, sz, output_size, self.back_buf.as_ref().unwrap().buffer_position, self.back_buf.as_ref().unwrap().start_parsing_position);
    }

    match res {
      SocketResult::Error | SocketResult::Closed => {
        self.frontend.write_error();
        self.log_request_error(metrics, "error writing to front socket, closing");
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.front_readiness.event.remove(Ready::writable());
      },
      SocketResult::Continue => {},
    }

    if !self.back_buf.as_ref().unwrap().can_restart_parsing() {
      self.back_readiness.interest.insert(Ready::readable());
      return SessionResult::Continue;
    }

    //handle this case separately as its cumbersome to do from the pattern match
    if let Some(sz) = self.must_continue_response() {
      self.front_readiness.interest.insert(Ready::readable());
      self.front_readiness.interest.remove(Ready::writable());

      if self.front_buf.is_none() {
        if let Some(p) = self.pool.upgrade() {
          if let Some(buf) = p.borrow_mut().checkout() {
            self.front_buf = Some(BufferQueue::with_buffer(buf));
          } else {
            error!("cannot get front buffer from pool, closing");
            return SessionResult::CloseSession;
          }
        }
      }

      // we must now copy the body from front to back
      trace!("100-Continue => copying {} of body from front to back", sz);
      self.front_buf.as_mut().map(|buf| {
        buf.slice_output(sz);
        buf.consume_parsed_data(sz);
      });

      self.response = Some(ResponseState::Initial);
      self.res_header_end = None;
      self.request.as_mut().map(|r| r.get_mut_connection().map(|conn| conn.continues = Continue::None));

      return SessionResult::Continue;
    }


    match self.response {
      // FIXME: should only restart parsing if we are using keepalive
      Some(ResponseState::Response(_,_))                            |
      Some(ResponseState::ResponseWithBody(_,_,_))                  |
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
        let front_keep_alive = self.request.as_ref().map(|r| r.should_keep_alive()).unwrap_or(false);
        let back_keep_alive  = self.response.as_ref().map(|r| r.should_keep_alive()).unwrap_or(false);

        save_http_status_metric(self.get_response_status());

        self.log_request_success(&metrics);
        metrics.reset();

        if self.closing {
          debug!("{} closing proxy, no keep alive", self.log_context());
          self.front_readiness.reset();
          self.back_readiness.reset();
          return SessionResult::CloseSession
        }

        //FIXME: we could get smarter about this
        // with no keepalive on backend, we could open a new backend ConnectionError
        // with no keepalive on front but keepalive on backend, we could have
        // a pool of connections
        if front_keep_alive && back_keep_alive {
          debug!("{} keep alive front/back", self.log_context());
          self.reset();
          self.front_readiness.interest = Ready::readable() | Ready::hup() | Ready::error();
          self.back_readiness.interest  = Ready::hup() | Ready::error();

          SessionResult::Continue
          //FIXME: issues reusing the backend socket
          //self.back_readiness.interest  = UnixReady::hup() | UnixReady::error();
          //SessionResult::CloseBackend
        } else if front_keep_alive && !back_keep_alive {
          debug!("{} keep alive front", self.log_context());
          self.reset();
          self.front_readiness.interest = Ready::readable() | Ready::hup() | Ready::error();
          self.back_readiness.interest  = Ready::hup() | Ready::error();
          SessionResult::CloseBackend(self.backend_token.take())
        } else {
          debug!("{} no keep alive", self.log_context());
          self.front_readiness.reset();
          self.back_readiness.reset();
          SessionResult::CloseSession
        }
      },
      Some(ResponseState::ResponseWithBodyCloseDelimited(_,_, back_closed)) => {
        self.back_readiness.interest.insert(Ready::readable());
        if back_closed {
          save_http_status_metric(self.get_response_status());
          self.log_request_success(&metrics);

          SessionResult::CloseSession
        } else {
          SessionResult::Continue
        }
      },
      // restart parsing, since there will be other chunks next
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        self.back_readiness.interest.insert(Ready::readable());
        SessionResult::Continue
      },
      //we're not done parsing the headers
      Some(ResponseState::HasStatusLine(_,_)) |
      Some(ResponseState::HasUpgrade(_,_,_))  |
      Some(ResponseState::HasLength(_,_,_))   => {
        self.back_readiness.interest.insert(Ready::readable());
        SessionResult::Continue
      },
      _ => {
        self.front_readiness.reset();
        self.back_readiness.reset();
        SessionResult::CloseSession
      }
    }
  }

  // Forward content to application
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      error!("{}\tsending default answer, should not write to back", self.log_context());
      self.back_readiness.interest.remove(Ready::writable());
      self.front_readiness.interest.insert(Ready::writable());
      return SessionResult::Continue;
    }

    if self.front_buf.as_ref().map(|buf| buf.output_data_size() == 0 || buf.next_output_data().is_empty()).unwrap() {
      self.front_readiness.interest.insert(Ready::readable());
      self.back_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    let tokens = self.tokens();
    let output_size = self.front_buf.as_ref().unwrap().output_data_size();
    if self.backend.is_none() {
      self.log_request_error(metrics, "back socket not found, closing connection");
      return SessionResult::CloseSession;
    }

    let mut sz = 0usize;
    let mut socket_res = SocketResult::Continue;

    {
      let sock = unwrap_msg!(self.backend.as_mut());
      while socket_res == SocketResult::Continue && self.front_buf.as_ref().unwrap().output_data_size() > 0 {
        // no more data in buffer, stop here
        if self.front_buf.as_ref().unwrap().next_output_data().is_empty() {
          self.front_readiness.interest.insert(Ready::readable());
          self.back_readiness.interest.remove(Ready::writable());
          metrics.backend_bout += sz;
          return SessionResult::Continue;
        }
        /*
        let (current_sz, current_res) = sock.socket_write(self.front_buf.as_ref().unwrap().next_output_data());
        */
        let bufs = self.front_buf.as_ref().unwrap().as_ioslice();
        if bufs.is_empty() {
          break;
        }
        let (current_sz, current_res) = sock.socket_write_vectored(&bufs);
        //println!("vectored io returned {:?}", (current_sz, current_res));
        socket_res = current_res;
        self.front_buf.as_mut().unwrap().consume_output_data(current_sz);
        sz += current_sz;
      }
    }

    metrics.backend_bout += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK [{}->{}]: wrote {} bytes of {}", self.log_context(), front.0, back.0, sz, output_size);
    }
    match socket_res {
      // the back socket is not writable anymore, so we can drop
      // the front buffer, no more data can be transmitted.
      // But the socket might still be readable, or if it is
      // closed, we might still have some data in the buffer.
      // As an example, we can get an early response for a large
      // POST request to refuse it and prevent all of the data
      // from being transmitted, awith the backend server closing
      // the socket right after sending the response
      SocketResult::Error | SocketResult::Closed => {
        self.front_buf = None;
        self.front_readiness.interest.remove(Ready::readable());
        self.back_readiness.interest.insert(Ready::readable());
        self.back_readiness.interest.remove(Ready::writable());
        return SessionResult::Continue;
      },
      SocketResult::WouldBlock => {
        self.back_readiness.event.remove(Ready::writable());

      },
      SocketResult::Continue => {}
    }

    // FIXME/ should read exactly as much data as needed
    if self.front_buf.as_ref().unwrap().can_restart_parsing() {
      match self.request {
        // the entire request was transmitted
        Some(RequestState::Request(_,_,_))                            |
        Some(RequestState::RequestWithBody(_,_,_,_))                  |
        Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
          // return the buffer to the pool
          // if there's still data in there, keep it for pipelining
          if self.must_continue_request() &&
            self.front_buf.as_ref().map(|buf| buf.empty()) == Some(true) {
              self.front_buf = None;
          }
          self.front_readiness.interest.remove(Ready::readable());
          self.back_readiness.interest.insert(Ready::readable());
          self.back_readiness.interest.remove(Ready::writable());

          // cancel the front timeout while we are waiting for the server to answer
          self.front_timeout.cancel();
          if let Some(token) = self.backend_token.as_ref() {
              self.back_timeout.set(*token);
          }
          SessionResult::Continue
        },
        Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Initial)) => {
          if !self.must_continue_request() {
            self.front_readiness.interest.insert(Ready::readable());
            SessionResult::Continue
          } else {
            // wait for the 100 continue response from the backend
            // keep the front buffer
            self.front_readiness.interest.remove(Ready::readable());
            self.back_readiness.interest.insert(Ready::readable());
            self.back_readiness.interest.remove(Ready::writable());
            SessionResult::Continue
          }
        }
        Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
          self.front_readiness.interest.insert(Ready::readable());
          SessionResult::Continue
        },
        //we're not done parsing the headers
        Some(RequestState::HasRequestLine(_,_))       |
        Some(RequestState::HasHost(_,_,_))            |
        Some(RequestState::HasLength(_,_,_))          |
        Some(RequestState::HasHostAndLength(_,_,_,_)) => {
          self.front_readiness.interest.insert(Ready::readable());
          SessionResult::Continue
        },
        _ => {
          self.log_request_error(metrics, "invalid state, closing connection");
          SessionResult::CloseSession
        }
      }
    } else {
      self.front_readiness.interest.insert(Ready::readable());
      self.back_readiness.interest.insert(Ready::writable());
      SessionResult::Continue
    }
  }

  // Read content from application
  pub fn back_readable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, SessionResult) {
    if !self.back_timeout.reset() {
        error!("could not reset back timeout {:?}:\n{}", self.back_timeout, self.print_state(""));
    }

    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      error!("{}\tsending default answer, should not read from back socket", self.log_context());
      self.back_readiness.interest.remove(Ready::readable());
      return (ProtocolResult::Continue, SessionResult::Continue);
    }

    if self.back_buf.is_none() {
      if let Some(p) = self.pool.upgrade() {
        if let Some(buf) = p.borrow_mut().checkout() {
          self.back_buf = Some(BufferQueue::with_buffer(buf));
        } else {
          error!("cannot get back buffer from pool, closing");
          return (ProtocolResult::Continue, SessionResult::CloseSession);
        }
      }
    }

    if self.back_buf.as_ref().unwrap().buffer.available_space() == 0 {
      self.back_readiness.interest.remove(Ready::readable());
      return (ProtocolResult::Continue, SessionResult::Continue);
    }

    let tokens     = self.tokens();

    if self.backend.is_none() {
      self.log_request_error(metrics, "back socket not found, closing connection");
      return (ProtocolResult::Continue, SessionResult::CloseSession);
    }

    let (sz, r) = {
      let sock = unwrap_msg!(self.backend.as_mut());
      sock.socket_read(&mut self.back_buf.as_mut().unwrap().buffer.space())
    };

    self.back_buf.as_mut().map(|back_buf| {
      back_buf.buffer.fill(sz);
      back_buf.sliced_input(sz);
    });

    metrics.backend_bin += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK  [{}<-{}]: read {} bytes", self.log_context(), front.0, back.0, sz);
    }

    if r != SocketResult::Continue || sz == 0 {
      self.back_readiness.event.remove(Ready::readable());
    }

    if r == SocketResult::Error {
      self.log_request_error(metrics, "back socket read error, closing connection");
      return (ProtocolResult::Continue, SessionResult::CloseSession);
    }

    // isolate that here because the "ref protocol" and the self.state = " make borrowing conflicts
    if let Some(ResponseState::ResponseUpgrade(_,_, ref protocol)) = self.response {
      debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
      if compare_no_case(protocol.as_bytes(), "websocket".as_bytes()) {
        self.front_timeout.reset();
        self.back_timeout.reset();
        return (ProtocolResult::Upgrade, SessionResult::Continue);
      } else {
        //FIXME: should we upgrade to a pipe or send an error?
        return (ProtocolResult::Continue, SessionResult::Continue);
      }
    }

    match self.response {
      Some(ResponseState::Response(_,_)) => {
        self.log_request_error(metrics, "should not go back in back_readable if the whole response was parsed");
        (ProtocolResult::Continue, SessionResult::CloseSession)
      },
      Some(ResponseState::ResponseWithBody(_,_,_)) => {
        self.front_readiness.interest.insert(Ready::writable());
        if ! self.back_buf.as_ref().unwrap().needs_input() {
          metrics.backend_stop();
          self.backend_stop = Some(Instant::now());
          self.back_readiness.interest.remove(Ready::readable());
        }
        (ProtocolResult::Continue, SessionResult::Continue)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
        use nom::HexDisplay;
        self.back_readiness.interest.remove(Ready::readable());
        if sz == 0 {
          (ProtocolResult::Continue, SessionResult::Continue)
        } else {
          error!("{}\tback read should have stopped on chunk ended\nreq: {:?} res:{:?}\ndata:{}",
            self.log_context(), self.request, self.response,
            self.back_buf.as_ref().unwrap().unparsed_data().to_hex(16));
          self.log_request_error(metrics, "back read should have stopped on chunk ended");
          (ProtocolResult::Continue, SessionResult::CloseSession)
        }
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Error)) => {
        self.log_request_error(metrics, "back read should have stopped on chunk error");
        (ProtocolResult::Continue, SessionResult::CloseSession)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        if ! self.back_buf.as_ref().unwrap().needs_input() {
          let (response_state, header_end, is_head) =
            (self.response.take().unwrap(), self.res_header_end.take(),
              self.request.as_ref().map(|request| request.is_head()).unwrap_or(false));

          {
            let sticky_session = self.sticky_session.as_ref().and_then(|session| {
              if self.should_add_sticky_header(session) {
                Some(session)
              } else {
                None
              }
            });

            let (response_state, header_end) = parse_response_until_stop(
              response_state, header_end, &mut self.back_buf.as_mut().unwrap(),
              is_head, &self.added_res_header,
              &self.sticky_name, sticky_session, self.app_id.as_deref());


            self.response = Some(response_state);
            self.res_header_end = header_end;
          }


          if unwrap_msg!(self.response.as_ref()).is_back_error() {
            self.log_request_error(metrics, "back socket chunk parse error, closing connection");
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          }

          if let Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) = self.response {
            metrics.backend_stop();
            self.backend_stop = Some(Instant::now());
            self.back_readiness.interest.remove(Ready::readable());
          }
        }
        self.front_readiness.interest.insert(Ready::writable());
        (ProtocolResult::Continue, SessionResult::Continue)
      },
      Some(ResponseState::ResponseWithBodyCloseDelimited(_,_,_)) => {
        self.front_readiness.interest.insert(Ready::writable());
        if sz > 0 {
          self.back_buf.as_mut().map(|buf| {
            buf.slice_output(sz);
            buf.consume_parsed_data(sz);
          });
        }

        if let ResponseState::ResponseWithBodyCloseDelimited(rl, conn, back_closed) = self.response.take().unwrap() {
          if r == SocketResult::Error || r == SocketResult::Closed || sz == 0 {
            self.response = Some(ResponseState::ResponseWithBodyCloseDelimited(rl, conn, true));

            // if the back buffer is already empty, we can stop here
            if self.back_buf.as_ref().map(|buf| buf.output_data_size() == 0 || buf.next_output_data().is_empty()).unwrap() {
              save_http_status_metric(self.get_response_status());
              self.log_request_success(&metrics);
              return (ProtocolResult::Continue, SessionResult::CloseSession);
            }
          } else {
            self.response = Some(ResponseState::ResponseWithBodyCloseDelimited(rl, conn, back_closed));
          }
        }

        (ProtocolResult::Continue, SessionResult::Continue)
      },
      Some(ResponseState::Error(_,_,_,_,_)) => panic!("{}\tback read should have stopped on responsestate error", self.log_context()),
      _ => {
        let (response_state, header_end, is_head) =
            (self.response.take().unwrap(), self.res_header_end.take(),
              self.request.as_ref().map(|request| request.is_head()).unwrap_or(false));

        {
          let sticky_session = self.sticky_session.as_ref().and_then(|session| {
            if self.should_add_sticky_header(session) {
              Some(session)
            } else {
              None
            }
          });

          let (response_state2, header_end2) = parse_response_until_stop(
            response_state, header_end, &mut self.back_buf.as_mut().unwrap(),
            is_head, &self.added_res_header,
            &self.sticky_name, sticky_session, self.app_id.as_deref());

          self.response = Some(response_state2);
          self.res_header_end = header_end2;
        };

        if unwrap_msg!(self.response.as_ref()).is_back_error() {
          self.set_answer(DefaultAnswerStatus::Answer502, None);
          return (ProtocolResult::Continue, self.writable(metrics));
        }

        if let Some(ResponseState::Response(_,_)) = self.response {
          metrics.backend_stop();
          self.backend_stop = Some(Instant::now());
          self.back_readiness.interest.remove(Ready::readable());
        }

        if let Some(ResponseState::ResponseUpgrade(_,_, ref protocol)) = self.response {
          debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
          if compare_no_case(protocol.as_bytes(), "websocket".as_bytes()) {
            self.front_timeout.reset();
            self.back_timeout.reset();
            return (ProtocolResult::Upgrade, SessionResult::Continue);
          } else {
            //FIXME: should we upgrade to a pipe or send an error?
            return (ProtocolResult::Continue, SessionResult::Continue);
          }
        }

        self.front_readiness.interest.insert(Ready::writable());
        (ProtocolResult::Continue, SessionResult::Continue)
      }
    }
  }

  // Check if the connection already has a sticky session header
  // The connection will have a sticky session header if the client sent one
  // If it's the same as the one we want to set, don't set it.
  // If the connection doesn't have a sticky session header or if it's different
  // from the one we want to set, then it should be set.
  fn should_add_sticky_header(&self, session: &StickySession) -> bool {
    self.request.as_ref()
      .and_then(|request| request.get_keep_alive())
      .and_then(|conn| conn.sticky_session.as_ref())
      .map(|sticky_client| sticky_client != &session.sticky_id)
      .unwrap_or(true)
  }

  pub fn timeout(&mut self, token: Token, metrics: &mut SessionMetrics) -> SessionResult {
    //info!("got timeout for token: {:?}", token);
    if self.frontend_token == token {
      self.front_timeout.triggered();
      match self.timeout_status() {
        TimeoutStatus::Request => {
          self.set_answer(DefaultAnswerStatus::Answer408, None);
          self.writable(metrics)
        },
        TimeoutStatus::WaitingForResponse => {
          self.set_answer(DefaultAnswerStatus::Answer504, None);
          self.writable(metrics)
        },
        TimeoutStatus::Response => {
          SessionResult::CloseSession
        },
        TimeoutStatus::WaitingForNewRequest => SessionResult::CloseSession,
      }
    } else if self.backend_token == Some(token) {
        //info!("backend timeout triggered for token {:?}", token);
        self.back_timeout.triggered();
        match self.timeout_status() {
            TimeoutStatus::Request => {
                error!("got backend timeout while waiting for a request, this should not happen");
                self.set_answer(DefaultAnswerStatus::Answer504, None);
                self.writable(metrics)
            },
            TimeoutStatus::WaitingForResponse => {
                self.set_answer(DefaultAnswerStatus::Answer504, None);
                self.writable(metrics)
            },
            TimeoutStatus::Response => {
                error!("backend {:?} timeout while receiving response (application {:?})",
                  self.backend_id, self.app_id);
                SessionResult::CloseSession
            },
            TimeoutStatus::WaitingForNewRequest => SessionResult::Continue,
        }
    } else {
        error!("got timeout for an invalid token");
        SessionResult::CloseSession
    }
  }

  pub fn cancel_timeouts(&mut self) {
      self.front_timeout.cancel();
      self.back_timeout.cancel();
  }

  pub fn cancel_backend_timeout(&mut self) {
      self.back_timeout.cancel();
  }
}

/// Save the backend http response status code metric
fn save_http_status_metric(rs_status_line : Option<&RStatusLine>) {
  if let Some(rs_status_line) = rs_status_line {
    match rs_status_line.status {
      100..=199 => { incr!("http.status.1xx"); },
      200..=299 => { incr!("http.status.2xx"); },
      300..=399 => { incr!("http.status.3xx"); },
      400..=499 => { incr!("http.status.4xx"); },
      500..=599 => { incr!("http.status.5xx"); },
      _ => { incr!("http.status.other"); }, // http responses with other codes (protocol error)
    }
  }
}

pub struct LogContext<'a> {
  pub request_id: Ulid,
  pub app_id:     Option<&'a str>,
  pub backend_id: Option<&'a str>,
}

impl<'a> std::fmt::Display for LogContext<'a> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{} {} {}\t",
      self.request_id,
      self.app_id.unwrap_or(&"-"),
      self.backend_id.unwrap_or(&"-")
    )
  }
}

struct SessionAddress(Option<SocketAddr>);

impl std::fmt::Display for SessionAddress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self.0 {
      None => write!(f, "X"),
      Some(SocketAddr::V4(addr)) => write!(f, "{}", addr),
      Some(SocketAddr::V6(addr)) => write!(f, "{}", addr),
    }
  }
}

struct OptionalString<'a> {
  inner: Option<&'a str>,
}

impl<'a> std::fmt::Display for OptionalString<'a> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.inner {
      None => write!(f, "-"),
      Some(s) => write!(f, "{}", s),
    }
  }
}

impl<'a> OptionalString<'a> {
  fn new(s: Option<&'a str>) -> Self {
    OptionalString { inner: s }
  }

  fn as_str(&self) -> &str {
    match &self.inner {
      None => "-",
      Some(s) => s,
    }
  }
}

struct OptionalRequest<'a>{
  inner: Option<(&'a parser::Method, &'a str)>
}

impl<'a> OptionalRequest<'a> {
  fn new(inner: Option<(&'a parser::Method, &'a str)>) -> Self {
    OptionalRequest { inner }
  }
}

impl<'a> std::fmt::Display for OptionalRequest<'a> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.inner {
      None => write!(f, "-"),
      Some((s1, s2)) => write!(f, "{} {}", s1, s2),
    }
  }
}

struct OptionalStatus<'a> {
  inner: Option<(u16, &'a str)>,
}

impl<'a> OptionalStatus<'a> {
  fn new(inner: Option<(u16, &'a str)>) -> Self {
    OptionalStatus { inner }
  }
}

impl<'a> std::fmt::Display for OptionalStatus<'a> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.inner {
      None => write!(f, "-"),
      Some((s1, s2)) => write!(f, "{} {}", s1, s2),
    }
  }
}

pub struct AddedRequestHeader {
    pub request_id: Ulid,
    pub public_address: SocketAddr,
    pub peer_address: Option<SocketAddr>,
    pub protocol: Protocol,
    pub closing: bool,
}

impl AddedRequestHeader {
    pub fn added_request_header(&self, headers: &parser::ForwardedHeaders) -> String {
        use std::fmt::Write;

        //FIXME: should update the Connection header directly if present
        let closing_header = if self.closing {
            "Connection: close\r\n"
        } else {
            ""
        };

        let front_ip = self.public_address.ip();
        let front_port = self.public_address.port();
        let proto = match self.protocol {
            Protocol::HTTP  => "http",
            Protocol::HTTPS => "https",
            _               => unreachable!()
        };

        let mut s = String::new();

        if !headers.x_proto {
            if let Err(e) = write!(&mut s, "X-Forwarded-Proto: {}\r\n", proto) {
                error!("could not append request header: {:?}", e);
            }
        }

        if !headers.x_port {
            if let Err(e) = write!(&mut s, "X-Forwarded-Port: {}\r\n", front_port) {
                error!("could not append request header: {:?}", e);
            }
        }

        if let Some(peer_addr) = self.peer_address {
            let peer_ip = peer_addr.ip();
            let peer_port = peer_addr.port();
            match &headers.x_for {
                None => {
                    if let Err(e) = write!(&mut s, "X-Forwarded-For: {}\r\n", peer_ip) {
                        error!("could not append request header: {:?}", e);
                    }
                },
                Some(value) => {
                    if let Err(e) = write!(&mut s, "X-Forwarded-For: {}, {}\r\n", value, peer_ip) {
                        error!("could not append request header: {:?}", e);
                    }
                },
            }

            match &headers.forwarded {
                None => {
                    if let Err(e) = write!(&mut s, "Forwarded: ") {
                        error!("could not append request header: {:?}", e);
                    }
                },
                Some(value) => {
                    if let Err(e) = write!(&mut s, "Forwarded: {}, ", value) {
                        error!("could not append request header: {:?}", e);
                    }
                },
            }

            match (peer_ip, peer_port, front_ip) {
                (IpAddr::V4(_), peer_port, IpAddr::V4(_)) => {
                    if let Err(e) = write!(&mut s, "proto={};for={}:{};by={}\r\n",
                           proto, peer_ip, peer_port, front_ip) {
                        error!("could not append request header: {:?}", e);
                    }
                },
                (IpAddr::V4(_), peer_port, IpAddr::V6(_)) => {
                    if let Err(e) = write!(&mut s, "proto={};for={}:{};by=\"{}\"\r\n",
                           proto, peer_ip, peer_port, front_ip) {
                        error!("could not append request header: {:?}", e);
                    }
                },
                (IpAddr::V6(_), peer_port, IpAddr::V4(_)) => {
                    if let Err(e) = write!(&mut s, "proto={};for=\"{}:{}\";by={}\r\n",
                           proto, peer_ip, peer_port, front_ip) {
                        error!("could not append request header: {:?}", e);
                    }
                },
                (IpAddr::V6(_), peer_port, IpAddr::V6(_)) => {
                    if let Err(e) = write!(&mut s, "proto={};for=\"{}:{}\";by=\"{}\"\r\n",
                           proto, peer_ip, peer_port, front_ip) {
                        error!("could not append request header: {:?}", e);
                    }
                },
            };
        }

        if let Err(e) = write!(&mut s, "Sozu-Id: {}\r\n{}", self.request_id, closing_header) {
            error!("could not append request header: {:?}", e);
        }

        s
    }
}

#[cfg(test)]
mod tests {
  use super::*;

  /*
  #[test]
  #[cfg(target_pointer_width = "64")]
  fn size_test() {
    assert_size!(SessionStatus, 24);
    assert_size!(String, 24);
    assert_size!(Option<String>, 24);
    assert_size!(DefaultAnswerStatus, 1);
    assert_size!(Readiness, 16);
    assert_size!(Option<BufferQueue>, 88);
    assert_size!(Option<SocketAddr>, 32);
    assert_size!(Option<RequestState>, 288);
    assert_size!(Option<ResponseState>, 256);
    assert_size!(Option<AddedRequestHeader>, 88);
  }
  */
}
