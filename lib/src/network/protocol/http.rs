use std::cmp::min;
use std::net::{SocketAddr,IpAddr};
use std::io::Write;
use mio::*;
use mio::unix::UnixReady;
use mio::tcp::TcpStream;
use pool::{Pool,Checkout,Reset};
use time::{Duration, precise_time_s, precise_time_ns};
use uuid::Uuid;
use parser::http11::{HttpState,parse_request_until_stop, parse_response_until_stop,
  BufferMove, RequestState, ResponseState, Chunk, Continue};
use network::{ClientResult,Protocol};
use network::buffer_queue::BufferQueue;
use network::session::Readiness;
use network::socket::{SocketHandler,SocketResult};
use network::protocol::ProtocolResult;
use util::UnwrapLog;

type BackendToken = Token;

#[derive(PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer,
}

pub struct Http<Front:SocketHandler> {
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  token:              Option<Token>,
  backend_token:      Option<Token>,
  rx_count:           usize,
  tx_count:           usize,
  pub status:         ClientStatus,
  pub state:          Option<HttpState>,
  pub front_buf:      Checkout<BufferQueue>,
  pub back_buf:       Checkout<BufferQueue>,
  front_buf_position: usize,
  back_buf_position:  usize,
  start:              u64,
  req_size:           usize,
  res_size:           usize,
  pub app_id:         Option<String>,
  pub request_id:     String,
  pub readiness:      Readiness,
  pub log_ctx:        String,
  pub public_address: Option<IpAddr>,
}

impl<Front:SocketHandler> Http<Front> {
  pub fn new(sock: Front, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>, public_address: Option<IpAddr>) -> Option<Http<Front>> {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    let log_ctx    = format!("{}\tunknown\t", &request_id);
    let mut client = Http {
      frontend:           sock,
      backend:            None,
      token:              None,
      backend_token:      None,
      rx_count:           0,
      tx_count:           0,
      status:             ClientStatus::Normal,
      state:              Some(HttpState::new()),
      front_buf:          front_buf,
      back_buf:           back_buf,
      front_buf_position: 0,
      back_buf_position:  0,
      start:              precise_time_ns(),
      req_size:           0,
      res_size:           0,
      app_id:             None,
      request_id:         request_id,
      readiness:          Readiness::new(),
      log_ctx:            log_ctx,
      public_address:     public_address,
    };
    let req_header = client.added_request_header(public_address);
    let res_header = client.added_response_header();
    client.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    client.state.as_mut().map(|ref mut state| state.added_res_header = res_header);

    Some(client)
  }

  pub fn reset(&mut self) {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    debug!("{} RESET TO {}", self.log_ctx, request_id);
    decr!("http.requests");
    self.state.as_mut().map(|state| state.reset());
    let req_header = self.added_request_header(self.public_address);
    let res_header = self.added_response_header();
    self.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    self.state.as_mut().map(|ref mut state| state.added_res_header = res_header);
    self.front_buf_position = 0;
    self.back_buf_position = 0;
    self.front_buf.reset();
    self.back_buf.reset();
    //self.readiness = Readiness::new();
    self.request_id = request_id;
    self.log_ctx = format!("{}\t{}\t", self.request_id, self.app_id.as_ref().unwrap_or(&String::from("unknown")));
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(front) = self.token {
      if let Some(back) = self.backend_token {
        return Some((front, back))
      }
    }
    None
  }

  pub fn state(&mut self) -> &mut HttpState {
    unwrap_msg!(self.state.as_mut())
  }

  pub fn set_state(&mut self, state: HttpState) {
    self.state = Some(state);
  }

  pub fn set_answer(&mut self, buf: &[u8])  {
    self.front_buf.reset();
    self.back_buf.reset();
    self.back_buf.write(buf);
    self.back_buf.consume_parsed_data(buf.len());
    self.back_buf.slice_output(buf.len());
    self.status = ClientStatus::DefaultAnswer;
  }

  pub fn added_request_header(&self, public_address: Option<IpAddr>) -> String {
    if let (Some(peer), Some(front)) = (
      self.front_socket().peer_addr().map(|addr| addr.ip()).ok(),
      public_address.or(self.front_socket().local_addr().map(|addr| addr.ip()).ok())
    ) {
      let proto = match self.protocol() {
        Protocol::HTTP  => "http",
        Protocol::HTTPS => "https",
        _               => unreachable!()
      };

      //FIXME: in the "for", we don't put the other values we could get from a preexisting forward header
      match (peer, front) {
        (IpAddr::V4(p), IpAddr::V4(f)) => {
          format!("Forwarded: proto={};for={};by={}\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\nRequest-id: {}\r\n",
            proto, peer, front, proto, peer, self.request_id)
        },
        (IpAddr::V4(p), IpAddr::V6(f)) => {
          format!("Forwarded: proto={};for={};by=\"{}\"\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\nRequest-id: {}\r\n",
            proto, peer, front, proto, peer, self.request_id)
        },
        (IpAddr::V6(p), IpAddr::V4(f)) => {
          format!("Forwarded: proto={};for=\"{}\";by={}\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\nRequest-id: {}\r\n",
            proto, peer, front, proto, peer, self.request_id)
        },
        (IpAddr::V6(p), IpAddr::V6(f)) => {
          format!("Forwarded: proto={};for=\"{}\";by=\"{}\"\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\nRequest-id: {}\r\n",
            proto, peer, front, proto, peer, self.request_id)
        },
      }
    } else {
      format!("Request-id: {}\r\n", self.request_id)
    }
  }

  pub fn added_response_header(&self) -> String {
    format!("Request-id: {}\r\n", self.request_id)
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn front_token(&self)  -> Option<Token> {
    self.token
  }

  pub fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  pub fn close(&mut self) {
  }

  pub fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t", self.request_id, app_id)
    } else {
      format!("{}\tunknown\t", self.request_id)
    }
  }

  pub fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend         = Some(socket);
  }

  pub fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  fn protocol(&self)           -> Protocol {
    Protocol::HTTP
  }

  pub fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.log_ctx, unwrap_msg!(self.token).0, unwrap_msg!(self.backend_token).0);
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  pub fn front_hup(&mut self) -> ClientResult {
    if self.backend_token == None {
      ClientResult::CloseClient
    } else {
      ClientResult::Continue
    }
  }

  pub fn back_hup(&mut self) -> ClientResult {
    if self.token == None {
      ClientResult::CloseClient
    } else {
      ClientResult::Continue
    }
  }

  // Read content from the client
  pub fn readable(&mut self) -> ClientResult {
    if self.status == ClientStatus::DefaultAnswer {
      self.readiness.front_interest.insert(Ready::writable());
      self.readiness.back_interest.remove(Ready::readable());
      self.readiness.back_interest.remove(Ready::writable());
      return ClientResult::Continue;
    }

    assert!(!unwrap_msg!(self.state.as_ref()).is_front_error());
    assert!(self.back_buf.empty(), "investigating single buffer usage: the back->front buffer should not be used while parsing and forwarding the request");

    if self.front_buf.buffer.available_space() == 0 {
      if self.backend_token == None {
        // We don't have a backend to empty the buffer into, close the connection
        error!("{}\t[{:?}] front buffer full, no backend, closing the connection", self.log_ctx, self.token);
        self.readiness.front_interest = UnixReady::from(Ready::empty());
        self.readiness.back_interest  = UnixReady::from(Ready::empty());
        return ClientResult::CloseClient;
      } else {
        self.readiness.front_interest.remove(Ready::readable());
        self.readiness.back_interest.insert(Ready::writable());
        return ClientResult::Continue;
      }
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.buffer.space());
    debug!("{}\tFRONT [{:?}]: read {} bytes", self.log_ctx, self.token, sz);

    if sz > 0 {
      self.front_buf.buffer.fill(sz);
      self.front_buf.sliced_input(sz);

      if self.front_buf.start_parsing_position > self.front_buf.parsed_position {
        let to_consume = min(self.front_buf.input_data_size(),
        self.front_buf.start_parsing_position - self.front_buf.parsed_position);
        self.front_buf.consume_parsed_data(to_consume);
      }

      if self.front_buf.buffer.available_space() == 0 {
        self.readiness.front_interest.remove(Ready::readable());
      }
    } else {
      self.readiness.front_readiness.remove(Ready::readable());
    }

    match res {
      SocketResult::Error => {
        error!("{}\t[{:?}] front socket error, closing the connection", self.log_ctx, self.token);
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(Ready::readable());
      },
      SocketResult::Continue => {}
    };

    // if there's no host, continue parsing until we find it
    let has_host = unwrap_msg!(self.state.as_ref()).has_host();
    if !has_host {
      self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
        &mut self.front_buf));
      if unwrap_msg!(self.state.as_ref()).is_front_error() {
        error!("{}\t[{:?}] front parsing error, closing the connection", self.log_ctx, self.token);
        //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
        self.readiness.front_interest.remove(Ready::readable());
        return ClientResult::CloseClient;
      }

      if unwrap_msg!(self.state.as_ref()).has_host() {
        self.readiness.back_interest.insert(Ready::writable());
        return ClientResult::ConnectBackend;
      } else {
        return ClientResult::Continue;
      }
    }

    if unwrap_msg!(self.state.as_ref()).request == Some(RequestState::Initial) {
      incr!("http.requests");
    }

    self.readiness.back_interest.insert(Ready::writable());
    match unwrap_msg!(self.state.as_ref()).request {
      Some(RequestState::Request(_,_,_)) | Some(RequestState::RequestWithBody(_,_,_,_)) => {
        if ! self.front_buf.needs_input() {
          // stop reading
          self.readiness.front_interest.remove(Ready::readable());
        }
        ClientResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
        error!("{}\t[{:?}] front read should have stopped on chunk ended", self.log_ctx, self.token);
        self.readiness.front_interest.remove(Ready::readable());
        ClientResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Error)) => {
        error!("{}\t[{:?}] front read should have stopped on chunk error", self.log_ctx, self.token);
        self.readiness.reset();
        ClientResult::CloseClient
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
        if ! self.front_buf.needs_input() {
          self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.front_buf));

          if unwrap_msg!(self.state.as_ref()).is_front_error() {
            error!("{}\t[{:?}] front chunk parsing error, closing the connection", self.log_ctx, self.token);
            //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
            self.readiness.reset();
            return ClientResult::CloseClient;
          }

          if let Some(&Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended))) = self.state.as_ref().map(|s| &s.request) {
            self.readiness.front_interest.remove(Ready::readable());
          }
        }
        self.readiness.back_interest.insert(Ready::writable());
        ClientResult::Continue
      },
    _ => {
        self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.front_buf));

        if unwrap_msg!(self.state.as_ref()).is_front_error() {
          error!("{}\t[{:?}] front parsing error, closing the connection", self.log_ctx, self.token);
          //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
          self.readiness.reset();
          return ClientResult::CloseClient;
        }

        if let Some(&Some(RequestState::Request(_,_,_))) = self.state.as_ref().map(|s| &s.request) {
          self.readiness.front_interest.remove(Ready::readable());
        }
        self.readiness.back_interest.insert(Ready::writable());
        ClientResult::Continue
      }
    }
  }

  // Forward content to client
  pub fn writable(&mut self) -> ClientResult {

    assert!(self.front_buf.empty(), "investigating single buffer usage: the front->back buffer should not be used while parsing and forwarding the response");

    let output_size = self.back_buf.output_data_size();
    if self.status == ClientStatus::DefaultAnswer {
      if self.back_buf.output_data_size() == 0 {
        self.readiness.front_interest.remove(Ready::writable());
      }

      let mut sz = 0usize;
      let mut res = SocketResult::Continue;
      while res == SocketResult::Continue && self.back_buf.output_data_size() > 0 {
        let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.next_output_data());
        res = current_res;
        self.back_buf.consume_output_data(current_sz);
        self.back_buf_position += current_sz;
        sz += current_sz;
      }

      if res != SocketResult::Continue {
        self.readiness.front_readiness.remove(Ready::writable());
      }

      if self.back_buf.buffer.available_data() == 0 {
        self.readiness.reset();
        error!("{}\t[{:?}] cannot write, back buffer was empty", self.log_ctx, self.token);
        return ClientResult::CloseClient;
      }

      if res == SocketResult::Error {
        self.readiness.reset();
        error!("{}\t[{:?}] error writing to front socket, closing", self.log_ctx, self.token);
        return ClientResult::CloseClient;
      } else {
        return ClientResult::Continue;
      }
    }

    if self.back_buf.output_data_size() == 0 || self.back_buf.next_output_data().len() == 0 {
      self.readiness.back_interest.insert(Ready::readable());
      self.readiness.front_interest.remove(Ready::writable());
      return ClientResult::Continue;
    }

    let mut sz = 0usize;
    let mut res = SocketResult::Continue;
    while res == SocketResult::Continue && self.back_buf.output_data_size() > 0 {
      // no more data in buffer, stop here
      if self.back_buf.next_output_data().len() == 0 {
        self.readiness.back_interest.insert(Ready::readable());
        self.readiness.front_interest.remove(Ready::writable());
        return ClientResult::Continue;
      }
      let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.next_output_data());
      res = current_res;
      self.back_buf.consume_output_data(current_sz);
      self.back_buf_position += current_sz;
      sz += current_sz;
    }

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}, buffer position {} restart position {}", self.log_ctx, front.0, back.0, sz, output_size, self.back_buf.buffer_position, self.back_buf.start_parsing_position);
    }

    match res {
      SocketResult::Error => {
        error!("{}\t[{:?}] error writing to front socket, closing", self.log_ctx, self.token);
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(Ready::writable());
      },
      SocketResult::Continue => {},
    }

    if !self.back_buf.can_restart_parsing() {
      self.readiness.back_interest.insert(Ready::readable());
      return ClientResult::Continue;
    }

    //handle this case separately as its cumbersome to do from the pattern match
    if let Some(sz) = self.state.as_ref().map(|st| st.must_continue()).unwrap_or(None) {
      self.readiness.front_interest.insert(Ready::readable());
      self.readiness.front_interest.remove(Ready::writable());

      // we must now copy the body from front to back
      trace!("100-Continue => copying {} of body from front to back", sz);
      self.front_buf.slice_output(sz);
      self.front_buf.consume_parsed_data(sz);
      return ClientResult::Continue;
    }

    match unwrap_msg!(self.state.as_ref()).response {
      // FIXME: should only restart parsing if we are using keepalive
      Some(ResponseState::Response(_,_))                            |
      Some(ResponseState::ResponseWithBody(_,_,_))                  |
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
        let front_keep_alive = self.state.as_ref().map(|st| st.request.as_ref().map(|r| r.should_keep_alive()).unwrap_or(false)).unwrap_or(false);
        let back_keep_alive  = self.state.as_ref().map(|st| st.response.as_ref().map(|r| r.should_keep_alive()).unwrap_or(false)).unwrap_or(false);

        //FIXME: we could get smarter about this
        // with no keepalive on backend, we could open a new backend ConnectionError
        // with no keepalive on front but keepalive on backend, we could have
        // a pool of connections
        if front_keep_alive && back_keep_alive {
          self.reset();
          self.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          self.readiness.back_interest  = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();

          info!("{}\t[{:?}] request ended successfully, keep alive for front and back", self.log_ctx, self.token);
          ClientResult::Continue
          //FIXME: issues reusing the backend socket
          //self.readiness.back_interest  = UnixReady::hup() | UnixReady::error();
          //ClientResult::CloseBackend
        } else if front_keep_alive && !back_keep_alive {
          self.reset();
          self.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          self.readiness.back_interest  = UnixReady::hup() | UnixReady::error();
          info!("{}\t[{:?}] request ended successfully, keepalive for front", self.log_ctx, self.token);
          ClientResult::CloseBackend
        } else {
          info!("{}\t[{:?}] request ended successfully, closing front and back connections", self.log_ctx, self.token);
          self.readiness.reset();
          ClientResult::CloseBothSuccess
        }
      },
      // restart parsing, since there will be other chunks next
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        self.readiness.back_interest.insert(Ready::readable());
        ClientResult::Continue
      },
      //we're not done parsing the headers
      Some(ResponseState::HasStatusLine(_,_)) |
      Some(ResponseState::HasUpgrade(_,_,_))  |
      Some(ResponseState::HasLength(_,_,_))   => {
        self.readiness.back_interest.insert(Ready::readable());
        ClientResult::Continue
      },
      _ => {
        self.readiness.reset();
        ClientResult::CloseBothFailure
      }
    }
  }

  // Forward content to application
  pub fn back_writable(&mut self) -> ClientResult {
    if self.status == ClientStatus::DefaultAnswer {
      error!("{}\tsending default answer, should not write to back", self.log_ctx);
      self.readiness.back_interest.remove(Ready::writable());
      self.readiness.front_interest.insert(Ready::writable());
      return ClientResult::Continue;
    }

    assert!(self.back_buf.empty(), "investigating single buffer usage: the back->front buffer should not be used while parsing and forwarding the request");

    if self.front_buf.output_data_size() == 0 || self.front_buf.next_output_data().len() == 0 {
      self.readiness.front_interest.insert(Ready::readable());
      self.readiness.back_interest.remove(Ready::writable());
      return ClientResult::Continue;
    }

    let tokens = self.tokens().clone();
    let output_size = self.front_buf.output_data_size();
    if self.backend.is_none() {
      error!("{}\tback socket not found, closing connection", self.log_ctx);
      self.readiness.reset();
      return ClientResult::CloseBothFailure;
    }

    let sock = unwrap_msg!(self.backend.as_mut());
    let mut sz = 0usize;
    let mut socket_res = SocketResult::Continue;

    while socket_res == SocketResult::Continue && self.front_buf.output_data_size() > 0 {
      // no more data in buffer, stop here
      if self.front_buf.next_output_data().len() == 0 {
        self.readiness.front_interest.insert(Ready::readable());
        self.readiness.back_interest.remove(Ready::writable());
        return ClientResult::Continue;
      }
      let (current_sz, current_res) = sock.socket_write(self.front_buf.next_output_data());
      socket_res = current_res;
      self.front_buf.consume_output_data(current_sz);
      self.front_buf_position += current_sz;
      sz += current_sz;
    }

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK [{}->{}]: wrote {} bytes of {}", self.log_ctx, front.0, back.0, sz, output_size);
    }
    match socket_res {
      SocketResult::Error => {
        error!("{}\tback socket write error, closing connection", self.log_ctx);
        self.readiness.reset();
        return ClientResult::CloseBothFailure;
      },
      SocketResult::WouldBlock => {
        self.readiness.back_readiness.remove(Ready::writable());

      },
      SocketResult::Continue => {}
    }

    // FIXME/ should read exactly as much data as needed
    //if self.front_buf_position >= self.state.req_position {
    if self.front_buf.can_restart_parsing() {
      match unwrap_msg!(self.state.as_ref()).request {
        Some(RequestState::Request(_,_,_))                            |
        Some(RequestState::RequestWithBody(_,_,_,_))                  |
        Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
          self.readiness.front_interest.remove(Ready::readable());
          self.readiness.back_interest.insert(Ready::readable());
          self.readiness.back_interest.remove(Ready::writable());
          ClientResult::Continue
        },
        Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
          self.readiness.front_interest.insert(Ready::readable());
          ClientResult::Continue
        },
        //we're not done parsing the headers
        Some(RequestState::HasRequestLine(_,_))       |
        Some(RequestState::HasHost(_,_,_))            |
        Some(RequestState::HasLength(_,_,_))          |
        Some(RequestState::HasHostAndLength(_,_,_,_)) => {
          self.readiness.front_interest.insert(Ready::readable());
          ClientResult::Continue
        },
        ref s => {
          error!("{}\tinvalid state, closing connection: {:?}", self.log_ctx, s);
          self.readiness.reset();
          ClientResult::CloseBothFailure
        }
      }
    } else {
      self.readiness.front_interest.insert(Ready::readable());
      self.readiness.back_interest.insert(Ready::writable());
      ClientResult::Continue
    }
  }

  // Read content from application
  pub fn back_readable(&mut self) -> (ProtocolResult, ClientResult) {
    if self.status == ClientStatus::DefaultAnswer {
      error!("{}\tsending default answer, should not read from back socket", self.log_ctx);
      self.readiness.back_interest.remove(Ready::readable());
      return (ProtocolResult::Continue, ClientResult::Continue);
    }

    assert!(self.front_buf.empty(), "investigating single buffer usage: the front->back buffer should not be used while parsing and forwarding the response");

    if self.back_buf.buffer.available_space() == 0 {
      self.readiness.back_interest.remove(Ready::readable());
      return (ProtocolResult::Continue, ClientResult::Continue);
    }

    let tokens     = self.tokens().clone();

    if self.backend.is_none() {
      error!("{}\tback socket not found, closing connection", self.log_ctx);
      self.readiness.reset();
      return (ProtocolResult::Continue, ClientResult::CloseBothFailure);
    }

    let sock = unwrap_msg!(self.backend.as_mut());
    let (sz, r) = sock.socket_read(&mut self.back_buf.buffer.space());
    self.back_buf.buffer.fill(sz);
    self.back_buf.sliced_input(sz);

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK  [{}<-{}]: read {} bytes", self.log_ctx, front.0, back.0, sz);
    }

    if r != SocketResult::Continue || sz == 0 {
      self.readiness.back_readiness.remove(Ready::readable());
    }

    if r == SocketResult::Error {
      error!("{}\tback socket read error, closing connection", self.log_ctx);
      self.readiness.reset();
      return (ProtocolResult::Continue, ClientResult::CloseBothFailure);
    }

    // isolate that here because the "ref protocol" and the self.state = " make borrowing conflicts
    if let Some(&Some(ResponseState::ResponseUpgrade(_,_, ref protocol))) = self.state.as_ref().map(|s| &s.response) {
      info!("got an upgrade state[{}]: {:?}", line!(), protocol);
      if protocol == "websocket" {
        return (ProtocolResult::Upgrade, ClientResult::Continue);
      } else {
        //FIXME: should we upgrade to a pipe or send an error?
        return (ProtocolResult::Continue, ClientResult::Continue);
      }
    }

    if let Some(sz) = self.state.as_ref().map(|st| st.must_continue()).unwrap_or(None) {
      trace!("100 continue wrote {} bytes to backend, resetting response state", sz);
      if let Some(ref mut state) = self.state.as_mut() {
        state.request.as_mut().map(|r| r.get_mut_connection().map(|mut conn| conn.continues = Continue::None));
        state.response       = Some(ResponseState::Initial);
        state.res_header_end = None;
      }
    }

    match unwrap_msg!(self.state.as_ref()).response {
      Some(ResponseState::Response(_,_)) => {
        error!("{}\tshould not go back in back_readable if the whole response was parsed", self.log_ctx);
        self.readiness.back_interest.remove(Ready::readable());
        (ProtocolResult::Continue, ClientResult::Continue)
      },
      Some(ResponseState::ResponseWithBody(_,_,_)) => {
        self.readiness.front_interest.insert(Ready::writable());
        if ! self.back_buf.needs_input() {
          self.readiness.back_interest.remove(Ready::readable());
        }
        (ProtocolResult::Continue, ClientResult::Continue)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
        error!("{}\tback read should have stopped on chunk ended", self.log_ctx);
        self.readiness.back_interest.remove(Ready::readable());
        (ProtocolResult::Continue, ClientResult::Continue)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Error)) => {
        error!("{}\tback read should have stopped on chunk error", self.log_ctx);
        self.readiness.reset();
        (ProtocolResult::Continue, ClientResult::CloseClient)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        if ! self.back_buf.needs_input() {
          self.state = Some(parse_response_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.back_buf));

          if unwrap_msg!(self.state.as_ref()).is_back_error() {
            error!("{}\tback socket chunk parse error, closing connection", self.log_ctx);
            //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
            self.readiness.reset();
            return (ProtocolResult::Continue, ClientResult::CloseBothFailure);
          }

          if let Some(&Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended))) = self.state.as_ref().map(|s| &s.response) {
            self.readiness.back_interest.remove(Ready::readable());
          }
        }
        self.readiness.front_interest.insert(Ready::writable());
        (ProtocolResult::Continue, ClientResult::Continue)
      },
      Some(ResponseState::Error(_)) => panic!("{}\tback read should have stopped on responsestate error", self.log_ctx),
      _ => {
        self.state = Some(parse_response_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
        &mut self.back_buf));

        if unwrap_msg!(self.state.as_ref()).is_back_error() {
          error!("{}\tback socket parse error, closing connection", self.log_ctx);
          //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
          self.readiness.reset();
          return (ProtocolResult::Continue, ClientResult::CloseBothFailure);
        }

        if let Some(ResponseState::Response(_,_)) = unwrap_msg!(self.state.as_ref()).response {
          self.readiness.back_interest.remove(Ready::readable());
        }

        if let Some(&Some(ResponseState::ResponseUpgrade(_,_, ref protocol))) = self.state.as_ref().map(|s| &s.response) {
          info!("got an upgrade state[{}]: {:?}", line!(), protocol);
          if protocol == "websocket" {
            return (ProtocolResult::Upgrade, ClientResult::Continue);
          } else {
            //FIXME: should we upgrade to a pipe or send an error?
            return (ProtocolResult::Continue, ClientResult::Continue);
          }
        }

        self.readiness.front_interest.insert(Ready::writable());
        (ProtocolResult::Continue, ClientResult::Continue)
      }
    }
  }
}


#[allow(non_snake_case)]
pub struct DefaultAnswers {
  pub NotFound:           Vec<u8>,
  pub ServiceUnavailable: Vec<u8>
}

