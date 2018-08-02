use std::cmp::min;
use std::net::{SocketAddr,IpAddr};
use std::io::Write;
use mio::*;
use mio::tcp::TcpStream;
use mio::unix::UnixReady;
use time::{Duration, precise_time_s, precise_time_ns};
use uuid::Uuid;
use network::{ClientResult,Readiness,SessionMetrics};
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult};
use network::pool::{Pool,Checkout,Reset};
use nom::HexDisplay;

type BackendToken = Token;

#[derive(PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer,
}

pub struct Pipe<Front:SocketHandler> {
  pub frontend:       Front,
  backend:            Option<TcpStream>,
  frontend_token:     Token,
  backend_token:      Option<Token>,
  pub front_buf:      Checkout<BufferQueue>,
  back_buf:           Checkout<BufferQueue>,
  front_buf_position: usize,
  back_buf_position:  usize,
  pub app_id:         Option<String>,
  pub request_id:     String,
  pub readiness:      Readiness,
  pub log_ctx:        String,
  public_address:     Option<IpAddr>,
}

impl<Front:SocketHandler> Pipe<Front> {
  pub fn new(frontend: Front, frontend_token: Token, backend: Option<TcpStream>, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>, public_address: Option<IpAddr>) -> Pipe<Front> {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    let log_ctx    = format!("{}\tunknown\t", &request_id);
    let client = Pipe {
      frontend:           frontend,
      backend:            backend,
      frontend_token:     frontend_token,
      backend_token:      None,
      front_buf:          front_buf,
      back_buf:           back_buf,
      front_buf_position: 0,
      back_buf_position:  0,
      app_id:             None,
      request_id:         request_id,
      readiness:          Readiness {
                            front_interest:  UnixReady::from(Ready::readable() | Ready::writable()) | UnixReady::hup() | UnixReady::error(),
                            back_interest:   UnixReady::from(Ready::readable() | Ready::writable()) | UnixReady::hup() | UnixReady::error(),
                            front_readiness: UnixReady::from(Ready::empty()),
                            back_readiness:  UnixReady::from(Ready::empty()),
      },
      log_ctx:            log_ctx,
      public_address:     public_address,
    };

    trace!("created pipe");
    client
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

  pub fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t", self.request_id, app_id)
    } else {
      format!("{}\tunknown\t", self.request_id)
    }
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  pub fn front_hup(&mut self) -> ClientResult {
    ClientResult::CloseClient
  }

  pub fn back_hup(&mut self) -> ClientResult {
    if self.back_buf.output_data_size() == 0 || self.back_buf.next_output_data().len() == 0 {
      if self.readiness.back_readiness.is_readable() {
        self.readiness().back_interest.insert(Ready::readable());
        error!("Pipe::back_hup: backend connection closed but the kernel still holds some data. readiness: {:?}", self.readiness);
        ClientResult::Continue
      } else {
        ClientResult::CloseClient
      }
    } else {
      self.readiness().front_interest.insert(Ready::writable());
      if self.readiness.back_readiness.is_readable() {
        self.readiness.back_interest.insert(Ready::readable());
      }
      ClientResult::Continue
    }
  }

  // Read content from the client
  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    trace!("pipe readable");
    if self.front_buf.buffer.available_space() == 0 {
      self.readiness.front_interest.remove(Ready::readable());
      self.readiness.back_interest.insert(Ready::writable());
      return ClientResult::Continue;
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.buffer.space());
    debug!("{}\tFRONT [{:?}]: read {} bytes", self.log_ctx, self.frontend_token, sz);

    if sz > 0 {
      //FIXME: replace with copy()
      self.front_buf.buffer.fill(sz);
      self.front_buf.sliced_input(sz);
      self.front_buf.consume_parsed_data(sz);
      self.front_buf.slice_output(sz);

      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      if self.front_buf.buffer.available_space() == 0 {
        self.readiness.front_interest.remove(Ready::readable());
      }
      self.readiness.back_interest.insert(Ready::writable());
    } else {
      self.readiness.front_readiness.remove(Ready::readable());
    }

    match res {
      SocketResult::Error => {
        error!("{}\t[{:?}] front socket error, closing the connection", self.log_ctx, self.frontend_token);
        metrics.service_stop();
        incr!("pipe.errors");
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::Closed => {
        metrics.service_stop();
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(Ready::readable());
      },
      SocketResult::Continue => {}
    };

    self.readiness.back_interest.insert(Ready::writable());
    ClientResult::Continue
  }

  // Forward content to client
  pub fn writable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    trace!("pipe writable");
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

    if sz > 0 {
      count!("bytes_out", sz as i64);
      self.readiness.back_interest.insert(Ready::readable());
      metrics.bout += sz;
    }

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}, buffer position {} restart position {}",
        self.log_ctx, front.0, back.0, sz, self.back_buf.output_data_size(),
        self.back_buf.buffer_position, self.back_buf.start_parsing_position);
    }

    match res {
      SocketResult::Error | SocketResult::Closed => {
        error!("{}\t[{:?}] error writing to front socket, closing", self.log_ctx, self.frontend_token);
        incr!("pipe.errors");
        metrics.service_stop();
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(Ready::writable());
      },
      SocketResult::Continue => {},
    }

    ClientResult::Continue
  }

  // Forward content to application
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    trace!("pipe back_writable");
    if self.front_buf.output_data_size() == 0 || self.front_buf.next_output_data().len() == 0 {
      self.readiness.front_interest.insert(Ready::readable());
      self.readiness.back_interest.remove(Ready::writable());
      return ClientResult::Continue;
    }

    let tokens = self.tokens().clone();
    let output_size = self.front_buf.output_data_size();

    let mut sz = 0usize;
    let mut socket_res = SocketResult::Continue;

    if let Some(ref mut backend) = self.backend {
      while socket_res == SocketResult::Continue && self.front_buf.output_data_size() > 0 {
        // no more data in buffer, stop here
        if self.front_buf.next_output_data().len() == 0 {
          self.readiness.front_interest.insert(Ready::readable());
          self.readiness.back_interest.remove(Ready::writable());
          return ClientResult::Continue;
        }

        let (current_sz, current_res) = backend.socket_write(self.front_buf.next_output_data());
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
        error!("{}\tback socket write error, closing connection", self.log_ctx);
        metrics.service_stop();
        incr!("pipe.errors");
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.back_readiness.remove(Ready::writable());

      },
      SocketResult::Continue => {}
    }
    ClientResult::Continue
  }

  // Read content from application
  pub fn back_readable(&mut self, metrics: &mut SessionMetrics) -> ClientResult {
    trace!("pipe back_readable");
    if self.back_buf.buffer.available_space() == 0 {
      self.readiness.back_interest.remove(Ready::readable());
      return ClientResult::Continue;
    }

    let tokens     = self.tokens().clone();

    if let Some(ref mut backend) = self.backend {
      let (sz, r) = backend.socket_read(&mut self.back_buf.buffer.space());
      self.back_buf.buffer.fill(sz);
      self.back_buf.sliced_input(sz);
      self.back_buf.consume_parsed_data(sz);
      self.back_buf.slice_output(sz);

      if let Some((front,back)) = tokens {
        debug!("{}\tBACK  [{}<-{}]: read {} bytes", self.log_ctx, front.0, back.0, sz);
      }

      if r != SocketResult::Continue || sz == 0 {
        self.readiness.back_readiness.remove(Ready::readable());
      }
      if sz > 0 {
        self.readiness.front_interest.insert(Ready::writable());
        metrics.backend_bin += sz;
      }

      match r {
        SocketResult::Error => {
          error!("{}\tback socket read error, closing connection", self.log_ctx);
          metrics.service_stop();
          incr!("pipe.errors");
          self.readiness.reset();
          return ClientResult::CloseClient;
        },
        SocketResult::Closed => {
          metrics.service_stop();
          self.readiness.reset();
          return ClientResult::CloseClient;
        },
        SocketResult::WouldBlock => {
          self.readiness.back_readiness.remove(Ready::readable());
        },
        SocketResult::Continue => {}
      }
    }

    ClientResult::Continue
  }
}

