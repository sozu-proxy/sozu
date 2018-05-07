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
  BufferMove, RequestState, ResponseState, Chunk, Continue, RRequestLine, RStatusLine};
use network::{ClientResult,Protocol,Readiness,SessionMetrics, LogDuration};
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult};
use network::protocol::ProtocolResult;
use util::UnwrapLog;

#[derive(Copy, Clone)]
pub struct StickySession {
  pub backend_id: u32
}

impl StickySession {
  pub fn new(backend_id: u32) -> StickySession {
    StickySession {
      backend_id: backend_id
    }
  }
}

type BackendToken = Token;

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer(DefaultAnswerStatus),
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum DefaultAnswerStatus {
  Answer301,
  Answer404,
  Answer503,
  Answer413,
}

pub struct Http<Front:SocketHandler> {
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  frontend_token:     Token,
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
  pub client_address: Option<SocketAddr>,
  pub sticky_session: Option<StickySession>,
  pub protocol:       Protocol,
}

impl<Front:SocketHandler> Http<Front> {
  pub fn new(sock: Front, token: Token, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>,
    public_address: Option<IpAddr>, client_address: Option<SocketAddr>, protocol: Protocol) -> Option<Http<Front>> {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    let log_ctx    = format!("{} unknown\t", &request_id);
    let mut client = Http {
      frontend:           sock,
      backend:            None,
      frontend_token:     token,
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
      client_address:     client_address,
      sticky_session:     None,
      protocol:           protocol,
    };
    let req_header = client.added_request_header(public_address, client_address);
    let res_header = client.added_response_header();
    client.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    client.state.as_mut().map(|ref mut state| state.added_res_header = res_header);

    Some(client)
  }

  pub fn reset(&mut self) {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    //info!("{} RESET TO {}", self.log_ctx, request_id);
    gauge_add!("http.active_requests", -1);
    self.state.as_mut().map(|state| state.reset());
    let req_header = self.added_request_header(self.public_address, self.client_address);
    let res_header = self.added_response_header();
    self.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    self.state.as_mut().map(|ref mut state| state.added_res_header = res_header);
    self.front_buf_position = 0;
    self.back_buf_position = 0;
    self.front_buf.reset();
    self.back_buf.reset();
    //self.readiness = Readiness::new();
    self.request_id = request_id;
    self.log_ctx = format!("{} {}\t",
      self.request_id, self.app_id.as_ref().unwrap_or(&String::from("unknown")));
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(back) = self.backend_token {
      return Some((self.frontend_token, back))
    }
    None
  }

  pub fn state(&mut self) -> &mut HttpState {
    unwrap_msg!(self.state.as_mut())
  }

  pub fn set_state(&mut self, state: HttpState) {
    self.state = Some(state);
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: &[u8])  {
    self.front_buf.reset();
    self.back_buf.reset();
    match self.back_buf.write(buf) {
      Ok(sz) => {
        self.back_buf.consume_parsed_data(sz);
        self.back_buf.slice_output(sz);

        if sz < buf.len() {
          error!("The backend buffer is too small, we couldn't write the entire answer");
        }
      }
      Err(e) => error!("The backend buffer is too small: {}", e),
    }

    self.status = ClientStatus::DefaultAnswer(answer);
  }

  pub fn added_request_header(&self, public_address: Option<IpAddr>, client_address: Option<SocketAddr>) -> String {
    let peer = client_address.or(self.front_socket().peer_addr().ok()).map(|addr| (addr.ip(), addr.port()));
    let front = public_address.or(self.front_socket().local_addr().map(|addr| addr.ip()).ok());
    if let (Some((peer_ip, peer_port)), Some(front)) = (peer, front) {
      let proto = match self.protocol() {
        Protocol::HTTP  => "http",
        Protocol::HTTPS => "https",
        _               => unreachable!()
      };

      //FIXME: in the "for", we don't put the other values we could get from a preexisting forward header
      match (peer_ip, peer_port, front) {
        (IpAddr::V4(p), peer_port, IpAddr::V4(f)) => {
          format!("Forwarded: proto={};for={}:{};by={}\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, peer_port, self.request_id)
        },
        (IpAddr::V4(p), peer_port, IpAddr::V6(f)) => {
          format!("Forwarded: proto={};for={}:{};by=\"{}\"\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, peer_port, self.request_id)
        },
        (IpAddr::V6(p), peer_port, IpAddr::V4(f)) => {
          format!("Forwarded: proto={};for=\"{}:{}\";by={}\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, peer_port, self.request_id)
        },
        (IpAddr::V6(p), peer_port, IpAddr::V6(f)) => {
          format!("Forwarded: proto={};for=\"{}:{}\";by=\"{}\"\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, peer_port, self.request_id)
        },
      }
    } else {
      format!("Sozu-Id: {}\r\n", self.request_id)
    }
  }

  pub fn added_response_header(&self) -> String {
    format!("Sozu-Id: {}\r\n", self.request_id)
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
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

  pub fn set_app_id(&mut self, app_id: String) {
    self.log_ctx = format!("{} {}\t",
      self.request_id, &app_id);

    self.app_id  = Some(app_id);
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  fn protocol(&self) -> Protocol {
    self.protocol
  }

  pub fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.log_ctx, self.frontend_token.0,
      self.backend_token.map(|t| format!("{}", t.0)).unwrap_or("-".to_string()));
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  pub fn front_hup(&mut self) -> ClientResult {
    ClientResult::CloseClient
  }

  pub fn back_hup(&mut self) -> ClientResult {
    //FIXME: closing the client might not be a good idea if we do keep alive on the front here?
    if self.back_buf.output_data_size() == 0 || self.back_buf.next_output_data().len() == 0 {
      ClientResult::CloseClient
    } else {
      ClientResult::Continue
    }
  }

  /// Retrieve the response status from the http response state
  pub fn get_response_status(&self) -> Option<RStatusLine> {
    if let Some(state) = self.state.as_ref() {
      state.get_status_line()
    }
    else {
      None
    }
  }

  pub fn get_host(&self) -> Option<String> {
    if let Some(state) = self.state.as_ref() {
      state.get_host()
    }
    else {
      None
    }
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    if let Some(state) = self.state.as_ref() {
      state.get_request_line()
    }
    else {
      None
    }
  }

  pub fn get_client_address(&self) -> Option<SocketAddr> {
    self.client_address.or(self.frontend.socket_ref().peer_addr().ok())
  }

  pub fn get_backend_address(&self) -> Option<SocketAddr> {
    self.backend.as_ref().and_then(|backend| backend.peer_addr().ok())
  }

  pub fn log_request_success(&self, metrics: &SessionMetrics) {
    let client = match self.get_client_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.get_backend_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let host         = self.get_host().unwrap_or(String::from("-"));
    let request_line = self.get_request_line().map(|line| format!("{} {}", line.method, line.uri)).unwrap_or(String::from("-"));
    let status_line  = self.get_response_status().map(|line| format!("{} {}", line.status, line.reason)).unwrap_or(String::from("-"));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    time!("request_time", &app_id, response_time.num_milliseconds());

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(), metrics.backend_bin, metrics.backend_bout);
      }
    }

    info_access!("{}{} -> {}\t{} {} {} {}\t{} {} {}",
      self.log_ctx, client, backend,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      status_line, host, request_line);
  }

  pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
    let client = match self.get_client_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let status_line = match self.status {
      ClientStatus::Normal => "-",
      ClientStatus::DefaultAnswer(DefaultAnswerStatus::Answer301) => "301 Moved Permanently",
      ClientStatus::DefaultAnswer(DefaultAnswerStatus::Answer404) => "404 Not Found",
      ClientStatus::DefaultAnswer(DefaultAnswerStatus::Answer503) => "503 Service Unavailable",
      ClientStatus::DefaultAnswer(DefaultAnswerStatus::Answer413) => "413 Payload Too Large",
    };

    let host         = self.get_host().unwrap_or(String::from("-"));
    let request_line = self.get_request_line().map(|line| format!("{} {}", line.method, line.uri)).unwrap_or(String::from("-"));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    if let Some(ref app_id) = self.app_id {
      time!("http.request.time", &app_id, response_time.num_milliseconds());
    }
    incr!("http.errors");

    info_access!("{}{} -> X\t{} {} {} {}\t{} {} {}",
      self.log_ctx, client,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      status_line, host, request_line);
  }

  pub fn log_request_error(&self, metrics: &SessionMetrics, message: &str) {
    let client = match self.get_client_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.get_backend_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let host         = self.get_host().unwrap_or(String::from("-"));
    let request_line = self.get_request_line().map(|line| format!("{} {}", line.method, line.uri)).unwrap_or(String::from("-"));
    let status_line  = self.get_response_status().map(|line| format!("{} {}", line.status, line.reason)).unwrap_or(String::from("-"));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    incr!("http.errors");
    /*time!("request_time", &app_id, response_time);

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(), metrics.backend_bin, metrics.backend_bout);
      }
    }*/

    error_access!("{}{} -> {}\t{} {} {} {}\t{} {} {} | {}",
      self.log_ctx, client, backend,
      LogDuration(response_time), LogDuration(service_time), metrics.bin, metrics.bout,
      status_line, host, request_line, message);
  }

  // Read content from the client
  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    if let ClientStatus::DefaultAnswer(_) = self.status {
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
        metrics.service_stop();
        self.log_request_error(metrics, "front buffer full, no backend, closing connection");
        let answer_413 = "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\n\r\n";
        self.set_answer(DefaultAnswerStatus::Answer413, answer_413.as_bytes());
        self.readiness.front_interest.remove(Ready::readable());
        self.readiness.front_interest.insert(Ready::writable());
      } else {
        self.readiness.front_interest.remove(Ready::readable());
        self.readiness.back_interest.insert(Ready::writable());
      }
      return ClientResult::Continue;
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.buffer.space());
    debug!("{}\tFRONT: read {} bytes", self.log_ctx, sz);

    if sz > 0 {
      self.front_buf.buffer.fill(sz);
      self.front_buf.sliced_input(sz);
      count!("bytes_in", sz as i64);
      metrics.bin += sz;

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
        self.log_request_error(metrics,
          &format!("front socket error, closing the connection. Readiness: {:?}", self.readiness));
        metrics.service_stop();
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::Closed => {
        //we were in keep alive but the peer closed the connection
        //FIXME: what happens if the connection was just opened but no data came?
        if unwrap_msg!(self.state.as_ref()).request == Some(RequestState::Initial) {
          metrics.service_stop();
          self.readiness.reset();
          return ClientResult::CloseClient;
        } else {
          self.log_request_error(metrics,
            &format!("front socket error, closing the connection. Readiness: {:?}", self.readiness));
          metrics.service_stop();
          self.readiness.reset();
          return ClientResult::CloseClient;
        }
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(Ready::readable());
      },
      SocketResult::Continue => {}
    };

    if unwrap_msg!(self.state.as_ref()).request == Some(RequestState::Initial) {
      gauge_add!("http.active_requests", 1);
      incr!("http.requests");
    }

    // if there's no host, continue parsing until we find it
    let has_host = unwrap_msg!(self.state.as_ref()).has_host();
    if !has_host {
      self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
        &mut self.front_buf));
      if unwrap_msg!(self.state.as_ref()).is_front_error() {
        self.log_request_error(metrics, "front parsing error, closing the connection");
        metrics.service_stop();
        //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
        self.readiness.front_interest.remove(Ready::readable());
        return ClientResult::CloseClient;
      }

      if unwrap_msg!(self.state.as_ref()).has_host() {
        self.readiness.back_interest.insert(Ready::writable());
        return ClientResult::ConnectBackend;
      } else {
        self.readiness.front_interest.insert(Ready::readable());
        return ClientResult::Continue;
      }
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
        error!("{}\tfront read should have stopped on chunk ended", self.log_ctx);
        self.readiness.front_interest.remove(Ready::readable());
        ClientResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Error)) => {
        self.log_request_error(metrics, "front read should have stopped on chunk error");
        metrics.service_stop();
        self.readiness.reset();
        ClientResult::CloseClient
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
        if ! self.front_buf.needs_input() {
          self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.front_buf));

          if unwrap_msg!(self.state.as_ref()).is_front_error() {
            self.log_request_error(metrics, "front chunk parsing error, closing the connection");
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
          self.log_request_error(metrics, "front parsing error, closing the connection");
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
  pub fn writable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {

    assert!(self.front_buf.empty(), "investigating single buffer usage: the front->back buffer should not be used while parsing and forwarding the response");

    //handle default answers
    let output_size = self.back_buf.output_data_size();
    if let ClientStatus::DefaultAnswer(answer) = self.status {
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

      count!("bytes_out", sz as i64);
      metrics.bout += sz;

      if res != SocketResult::Continue {
        self.readiness.front_readiness.remove(Ready::writable());
      }

      if self.back_buf.buffer.available_data() == 0 {
        metrics.service_stop();
        self.log_default_answer_success(&metrics);
        self.readiness.reset();
        return ClientResult::CloseClient;
      }

      if res == SocketResult::Error {
        self.readiness.reset();
        metrics.service_stop();
        self.log_request_error(metrics, "error writing default answer to front socket, closing");
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
        count!("bytes_out", sz as i64);
        metrics.bout += sz;
        return ClientResult::Continue;
      }
      let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.next_output_data());
      res = current_res;
      self.back_buf.consume_output_data(current_sz);
      self.back_buf_position += current_sz;
      sz += current_sz;
    }
    count!("bytes_out", sz as i64);
    metrics.bout += sz;

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}, buffer position {} restart position {}", self.log_ctx, front.0, back.0, sz, output_size, self.back_buf.buffer_position, self.back_buf.start_parsing_position);
    }

    match res {
      SocketResult::Error | SocketResult::Closed => {
        metrics.service_stop();
        self.log_request_error(metrics, "error writing to front socket, closing");
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

        save_http_status_metric(self.get_response_status());

        self.log_request_success(&metrics);
        metrics.reset();
        //FIXME: we could get smarter about this
        // with no keepalive on backend, we could open a new backend ConnectionError
        // with no keepalive on front but keepalive on backend, we could have
        // a pool of connections
        if front_keep_alive && back_keep_alive {
          debug!("{} keep alive front/back", self.log_ctx);
          self.reset();
          self.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          self.readiness.back_interest  = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
          
          ClientResult::Continue
          //FIXME: issues reusing the backend socket
          //self.readiness.back_interest  = UnixReady::hup() | UnixReady::error();
          //ClientResult::CloseBackend
        } else if front_keep_alive && !back_keep_alive {
          debug!("{} keep alive front", self.log_ctx);
          self.reset();
          self.readiness.front_interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          self.readiness.back_interest  = UnixReady::hup() | UnixReady::error();
          ClientResult::CloseBackend(self.backend_token.clone())
        } else {
          debug!("{} no keep alive", self.log_ctx);
          self.readiness.reset();
          ClientResult::CloseClient
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
        ClientResult::CloseClient
      }
    }
  }

  // Forward content to application
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    if let ClientStatus::DefaultAnswer(_) = self.status {
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
      metrics.service_stop();
      self.log_request_error(metrics, "back socket not found, closing connection");
      self.readiness.reset();
      return ClientResult::CloseClient;
    }

    let mut sz = 0usize;
    let mut socket_res = SocketResult::Continue;

    {
      let sock = unwrap_msg!(self.backend.as_mut());
      while socket_res == SocketResult::Continue && self.front_buf.output_data_size() > 0 {
        // no more data in buffer, stop here
        if self.front_buf.next_output_data().len() == 0 {
          self.readiness.front_interest.insert(Ready::readable());
          self.readiness.back_interest.remove(Ready::writable());
          metrics.backend_bout += sz;
          return ClientResult::Continue;
        }
        let (current_sz, current_res) = sock.socket_write(self.front_buf.next_output_data());
        socket_res = current_res;
        self.front_buf.consume_output_data(current_sz);
        self.front_buf_position += current_sz;
        sz += current_sz;
      }
    }

    metrics.backend_bout += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK [{}->{}]: wrote {} bytes of {}", self.log_ctx, front.0, back.0, sz, output_size);
    }
    match socket_res {
      SocketResult::Error | SocketResult::Closed => {
        metrics.service_stop();
        self.log_request_error(metrics, "back socket write error, closing connection");
        self.readiness.reset();
        return ClientResult::CloseClient;
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
          metrics.service_stop();
          self.log_request_error(metrics, "invalid state, closing connection");
          self.readiness.reset();
          ClientResult::CloseClient
        }
      }
    } else {
      self.readiness.front_interest.insert(Ready::readable());
      self.readiness.back_interest.insert(Ready::writable());
      ClientResult::Continue
    }
  }

  // Read content from application
  pub fn back_readable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, ClientResult) {
    if let ClientStatus::DefaultAnswer(_) = self.status {
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
      metrics.service_stop();
      self.log_request_error(metrics, "back socket not found, closing connection");
      self.readiness.reset();
      return (ProtocolResult::Continue, ClientResult::CloseClient);
    }

    let (sz, r) = {
      let sock = unwrap_msg!(self.backend.as_mut());
      sock.socket_read(&mut self.back_buf.buffer.space())
    };

    self.back_buf.buffer.fill(sz);
    self.back_buf.sliced_input(sz);

    metrics.backend_bin += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK  [{}<-{}]: read {} bytes", self.log_ctx, front.0, back.0, sz);
    }

    if r != SocketResult::Continue || sz == 0 {
      self.readiness.back_readiness.remove(Ready::readable());
    }

    if r == SocketResult::Error {
      metrics.service_stop();
      self.log_request_error(metrics, "back socket read error, closing connection");
      self.readiness.reset();
      return (ProtocolResult::Continue, ClientResult::CloseClient);
    }

    // isolate that here because the "ref protocol" and the self.state = " make borrowing conflicts
    if let Some(&Some(ResponseState::ResponseUpgrade(_,_, ref protocol))) = self.state.as_ref().map(|s| &s.response) {
      debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
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
        state.request.as_mut().map(|r| r.get_mut_connection().map(|conn| conn.continues = Continue::None));
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
        self.log_request_error(metrics, "back read should have stopped on chunk error");
        self.readiness.reset();
        (ProtocolResult::Continue, ClientResult::CloseClient)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        if ! self.back_buf.needs_input() {
          self.state = Some(parse_response_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.back_buf, self.sticky_session.take()));

          if unwrap_msg!(self.state.as_ref()).is_back_error() {
            metrics.service_stop();
            self.log_request_error(metrics, "back socket chunk parse error, closing connection");
            //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
            self.readiness.reset();
            return (ProtocolResult::Continue, ClientResult::CloseClient);
          }

          if let Some(&Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended))) = self.state.as_ref().map(|s| &s.response) {
            self.readiness.back_interest.remove(Ready::readable());
          }
        }
        self.readiness.front_interest.insert(Ready::writable());
        (ProtocolResult::Continue, ClientResult::Continue)
      },
      Some(ResponseState::Error(_,_,_,_,_)) => panic!("{}\tback read should have stopped on responsestate error", self.log_ctx),
      _ => {
        self.state = Some(parse_response_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
        &mut self.back_buf, self.sticky_session.take()));

        if unwrap_msg!(self.state.as_ref()).is_back_error() {
          metrics.service_stop();
          self.log_request_error(metrics, "back socket parse error, closing connection");
          //time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
          self.readiness.reset();
          return (ProtocolResult::Continue, ClientResult::CloseClient);
        }

        if let Some(ResponseState::Response(_,_)) = unwrap_msg!(self.state.as_ref()).response {
          self.readiness.back_interest.remove(Ready::readable());
        }

        if let Some(&Some(ResponseState::ResponseUpgrade(_,_, ref protocol))) = self.state.as_ref().map(|s| &s.response) {
          debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
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

/// Save the backend http response status code metric
fn save_http_status_metric(rs_status_line : Option<RStatusLine>) {
  if let Some(rs_status_line) = rs_status_line {
    match rs_status_line.status {
      100...199 => { incr!("http.status.1xx"); },
      200...299 => { incr!("http.status.2xx"); },
      300...399 => { incr!("http.status.3xx"); },
      400...499 => { incr!("http.status.4xx"); },
      500...599 => { incr!("http.status.5xx"); },
      _ => { incr!("http.status.other"); }, // http responses with other codes (protocol error)
    }
  }
}
