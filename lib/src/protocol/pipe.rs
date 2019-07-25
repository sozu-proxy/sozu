use std::net::SocketAddr;
use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use uuid::adapter::Hyphenated;
use sozu_command::buffer::Buffer;
use {SessionResult,Readiness,SessionMetrics};
use socket::{SocketHandler,SocketResult};
use pool::Checkout;
use {Protocol, LogDuration};

#[derive(PartialEq)]
pub enum SessionStatus {
  Normal,
  DefaultAnswer,
}

pub struct Pipe<Front:SocketHandler> {
  pub frontend:       Front,
  backend:            Option<TcpStream>,
  frontend_token:     Token,
  backend_token:      Option<Token>,
  pub front_buf:      Checkout<Buffer>,
  back_buf:           Checkout<Buffer>,
  pub app_id:         Option<String>,
  pub request_id:     Hyphenated,
  pub front_readiness:Readiness,
  pub back_readiness: Readiness,
  pub log_ctx:        String,
  session_address:    Option<SocketAddr>,
  protocol:           Protocol,
}

impl<Front:SocketHandler> Pipe<Front> {
  pub fn new(frontend: Front, frontend_token: Token, request_id: Hyphenated,
    backend: Option<TcpStream>, front_buf: Checkout<Buffer>,
    back_buf: Checkout<Buffer>, session_address: Option<SocketAddr>, protocol: Protocol) -> Pipe<Front> {
    let log_ctx    = format!("{}\tunknown\t", &request_id);
    let session = Pipe {
      frontend,
      backend,
      frontend_token,
      backend_token:      None,
      front_buf,
      back_buf,
      app_id:             None,
      request_id,
      front_readiness:    Readiness {
                            interest:  UnixReady::from(Ready::readable() | Ready::writable()) | UnixReady::hup() | UnixReady::error(),
                            event: UnixReady::from(Ready::empty()),
      },
      back_readiness:    Readiness {
                            interest:  UnixReady::from(Ready::readable() | Ready::writable()) | UnixReady::hup() | UnixReady::error(),
                            event: UnixReady::from(Ready::empty()),
      },
      log_ctx,
      session_address,
      protocol,
    };

    trace!("created pipe");
    session
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(back) = self.backend_token {
      return Some((self.frontend_token, back))
    }
    None
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend = Some(socket);
  }

  pub fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  pub fn close(&mut self) {
  }

  pub fn set_app_id(&mut self, app_id: Option<String>) {
    if let Some(ref app_id) = app_id {
      self.log_ctx = format!("{} {}\t", self.request_id, app_id);
    } else {
      self.log_ctx = format!("{} unknown\t", self.request_id);
    }

    self.app_id = app_id;
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn front_readiness(&mut self) -> &mut Readiness {
    &mut self.front_readiness
  }

  pub fn back_readiness(&mut self) -> &mut Readiness {
    &mut self.back_readiness
  }

  pub fn get_session_address(&self) -> Option<SocketAddr> {
    self.session_address.or_else(|| self.frontend.socket_ref().peer_addr().ok())
  }

  pub fn get_backend_address(&self) -> Option<SocketAddr> {
    self.backend.as_ref().and_then(|backend| backend.peer_addr().ok())
  }

  pub fn log_request_success(&self, metrics: &SessionMetrics) {
    let session = match self.get_session_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.get_backend_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let app_id = self.app_id.clone().unwrap_or_else(|| String::from("-"));
    time!("request_time", &app_id, response_time.num_milliseconds());

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(),
          metrics.backend_connection_time(), metrics.backend_bin, metrics.backend_bout);
      }
    }

    let proto = match self.protocol {
      Protocol::HTTP  => "HTTP+WS",
      Protocol::HTTPS => "HTTPS+WS",
      Protocol::TCP   => "TCP",
      _               => unreachable!()
    };

    info_access!("{}{} -> {}\t{} {} {} {}\t{}",
      self.log_ctx, session, backend,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      proto);
  }

  pub fn log_request_error(&self, metrics: &SessionMetrics, message: &str) {
    let session = match self.get_session_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.get_backend_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let app_id = self.app_id.clone().unwrap_or_else(|| String::from("-"));
    time!("request_time", &app_id, response_time.num_milliseconds());

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(),
          metrics.backend_connection_time(), metrics.backend_bin, metrics.backend_bout);
      }
    }

    let proto = match self.protocol {
      Protocol::HTTP  => "HTTP+WS",
      Protocol::HTTPS => "HTTPS+WS",
      Protocol::TCP   => "TCP",
      _               => unreachable!()
    };

    error_access!("{}{} -> {}\t{} {} {} {}\t{} | {}",
      self.log_ctx, session, backend,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      proto, message);
  }

  pub fn front_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    self.log_request_success(metrics);
    SessionResult::CloseSession
  }

  pub fn back_hup(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    if self.back_buf.available_data() == 0 {
      if self.back_readiness.event.is_readable() {
        self.back_readiness().interest.insert(Ready::readable());
        error!("Pipe::back_hup: backend connection closed but the kernel still holds some data. readiness: {:?} -> {:?}", self.front_readiness, self.back_readiness);
        SessionResult::Continue
      } else {
        self.log_request_success(metrics);
        SessionResult::CloseSession
      }
    } else {
      self.front_readiness().interest.insert(Ready::writable());
      if self.back_readiness.event.is_readable() {
        self.back_readiness.interest.insert(Ready::readable());
      }
      SessionResult::Continue
    }
  }

  // Read content from the session
  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    trace!("pipe readable");
    if self.front_buf.available_space() == 0 {
      self.front_readiness.interest.remove(Ready::readable());
      self.back_readiness.interest.insert(Ready::writable());
      return SessionResult::Continue;
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.space());
    debug!("{}\tFRONT [{:?}]: read {} bytes", self.log_ctx, self.frontend_token, sz);

    if sz > 0 {
      //FIXME: replace with copy()
      self.front_buf.fill(sz);

      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      if self.front_buf.available_space() == 0 {
        self.front_readiness.interest.remove(Ready::readable());
      }
      self.back_readiness.interest.insert(Ready::writable());
    } else {
      self.front_readiness.event.remove(Ready::readable());
    }

    match res {
      SocketResult::Error => {
        metrics.service_stop();
        incr!("pipe.errors");
        self.front_readiness.reset();
        self.back_readiness.reset();
        self.log_request_error(metrics, "front socket read error");
        return SessionResult::CloseSession;
      },
      SocketResult::Closed => {
        metrics.service_stop();
        self.front_readiness.reset();
        self.back_readiness.reset();
        self.log_request_success(metrics);
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.front_readiness.event.remove(Ready::readable());
      },
      SocketResult::Continue => {}
    };

    self.back_readiness.interest.insert(Ready::writable());
    SessionResult::Continue
  }

  // Forward content to session
  pub fn writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    trace!("pipe writable");
    if self.back_buf.available_data() == 0 {
      self.back_readiness.interest.insert(Ready::readable());
      self.front_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    let mut sz = 0usize;
    let mut res = SocketResult::Continue;
    while res == SocketResult::Continue {
      // no more data in buffer, stop here
      if self.back_buf.available_data() == 0 {
        count!("bytes_out", sz as i64);
        metrics.bout += sz;
        self.back_readiness.interest.insert(Ready::readable());
        self.front_readiness.interest.remove(Ready::writable());
        return SessionResult::Continue;
      }
      let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.data());
      res = current_res;
      self.back_buf.consume(current_sz);
      sz += current_sz;
    }

    if sz > 0 {
      count!("bytes_out", sz as i64);
      self.back_readiness.interest.insert(Ready::readable());
      metrics.bout += sz;
    }

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}",
        self.log_ctx, front.0, back.0, sz, self.back_buf.available_data());
    }

    match res {
      SocketResult::Error => {
        incr!("pipe.errors");
        metrics.service_stop();
        self.front_readiness.reset();
        self.back_readiness.reset();
        self.log_request_error(metrics, "front socket write error");
        return SessionResult::CloseSession;
      },
      SocketResult::Closed => {
        metrics.service_stop();
        self.front_readiness.reset();
        self.back_readiness.reset();
        self.log_request_success(metrics);
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.front_readiness.event.remove(Ready::writable());
      },
      SocketResult::Continue => {},
    }

    SessionResult::Continue
  }

  // Forward content to application
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    trace!("pipe back_writable");
    if self.front_buf.available_data() == 0 {
      self.front_readiness.interest.insert(Ready::readable());
      self.back_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    let tokens = self.tokens();
    let output_size = self.front_buf.available_data();

    let mut sz = 0usize;
    let mut socket_res = SocketResult::Continue;

    if let Some(ref mut backend) = self.backend {
      while socket_res == SocketResult::Continue {
        // no more data in buffer, stop here
        if self.front_buf.available_data() == 0 {
          self.front_readiness.interest.insert(Ready::readable());
          self.back_readiness.interest.remove(Ready::writable());
          return SessionResult::Continue;
        }

        let (current_sz, current_res) = backend.socket_write(self.front_buf.data());
        socket_res = current_res;
        self.front_buf.consume(current_sz);
        sz += current_sz;
      }
    }

    metrics.backend_bout += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK [{}->{}]: wrote {} bytes of {}", self.log_ctx, front.0, back.0, sz, output_size);
    }
    match socket_res {
      SocketResult::Error => {
        metrics.service_stop();
        incr!("pipe.errors");
        self.front_readiness.reset();
        self.back_readiness.reset();
        self.log_request_error(metrics, "back socket write error");
        return SessionResult::CloseSession;
      },
      SocketResult::Error | SocketResult::Closed => {
        metrics.service_stop();
        self.front_readiness.reset();
        self.back_readiness.reset();
        self.log_request_success(metrics);
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.back_readiness.event.remove(Ready::writable());

      },
      SocketResult::Continue => {}
    }
    SessionResult::Continue
  }

  // Read content from application
  pub fn back_readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    trace!("pipe back_readable");
    if self.back_buf.available_space() == 0 {
      self.back_readiness.interest.remove(Ready::readable());
      return SessionResult::Continue;
    }

    let tokens     = self.tokens();

    if let Some(ref mut backend) = self.backend {
      let (sz, r) = backend.socket_read(&mut self.back_buf.space());
      self.back_buf.fill(sz);

      if let Some((front,back)) = tokens {
        debug!("{}\tBACK  [{}<-{}]: read {} bytes", self.log_ctx, front.0, back.0, sz);
      }

      if r != SocketResult::Continue || sz == 0 {
        self.back_readiness.event.remove(Ready::readable());
      }
      if sz > 0 {
        self.front_readiness.interest.insert(Ready::writable());
        metrics.backend_bin += sz;
      }

      match r {
        SocketResult::Error => {
          metrics.service_stop();
          incr!("pipe.errors");
          self.front_readiness.reset();
          self.back_readiness.reset();
          self.log_request_error(metrics, "back socket read error");
          return SessionResult::CloseSession;
        },
        SocketResult::Closed => {
          metrics.service_stop();
          self.front_readiness.reset();
          self.back_readiness.reset();
          self.log_request_success(metrics);
          return SessionResult::CloseSession;
        },
        SocketResult::WouldBlock => {
          self.back_readiness.event.remove(Ready::readable());
        },
        SocketResult::Continue => {}
      }
    }

    SessionResult::Continue
  }
}

