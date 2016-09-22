#![allow(unused_imports)]

use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::cmp::min;
use mio::tcp::*;
use std::io::{self,Read,Write,ErrorKind};
use mio::*;
use bytes::{ByteBuf,MutByteBuf};
use bytes::buf::MutBuf;
use pool::{Pool,Checkout,Reset};
use std::collections::HashMap;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::{FromStr, from_utf8};
use time::{Duration, precise_time_s, precise_time_ns};
use rand::random;
use uuid::Uuid;
use network::{Backend,ClientResult,ServerMessage,ServerMessageType,ConnectionError,ProxyOrder,RequiredEvents};
use network::proxy::{Server,ProxyConfiguration,ProxyClient,Readiness};
use network::buffer::Buffer;
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult,server_bind};

use parser::http11::{HttpState,parse_request_until_stop, parse_response_until_stop, BufferMove, RequestState, ResponseState, Chunk};
use nom::HexDisplay;

use messages::{Command,HttpFront};

type BackendToken = Token;

#[derive(PartialEq)]
pub enum ClientStatus {
  Normal,
  DefaultAnswer,
}

pub struct Client<Front:SocketHandler> {
  frontend:       Front,
  backend:        Option<TcpStream>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  front_timeout:  Option<Timeout>,
  back_timeout:   Option<Timeout>,
  rx_count:       usize,
  tx_count:       usize,
  status:         ClientStatus,

  state:              Option<HttpState>,
  front_buf:          Checkout<BufferQueue>,
  back_buf:           Checkout<BufferQueue>,
  front_buf_position: usize,
  back_buf_position:  usize,
  start:              u64,
  req_size:           usize,
  res_size:           usize,
  pub app_id:         Option<String>,
  request_id:         String,
  server_context:     String,
  readiness:          Readiness,
}

impl<Front:SocketHandler> Client<Front> {
  pub fn new(server_context: &str, sock: Front, front_buf: Checkout<BufferQueue>, back_buf: Checkout<BufferQueue>) -> Option<Client<Front>> {
    let mut client = Client {
      frontend:       sock,
      backend:        None,
      token:          None,
      backend_token:  None,
      front_timeout:  None,
      back_timeout:   None,
      rx_count:       0,
      tx_count:       0,
      status:         ClientStatus::Normal,

      state:              Some(HttpState::new()),
      front_buf:          front_buf,
      back_buf:           back_buf,
      front_buf_position: 0,
      back_buf_position:  0,
      start:              precise_time_ns(),
      req_size:           0,
      res_size:           0,
      app_id:             None,
      request_id:         Uuid::new_v4().hyphenated().to_string(),
      server_context:     String::from(server_context),
      readiness:          Readiness::new(),
    };
    let req_header = client.added_request_header();
    let res_header = client.added_response_header();
    client.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    client.state.as_mut().map(|ref mut state| state.added_res_header = res_header);

    Some(client)
  }

  pub fn close(&self) {
  }

  pub fn reset(&mut self) {
    self.state.as_mut().unwrap().reset();
    let req_header = self.added_request_header();
    let res_header = self.added_response_header();
    self.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    self.state.as_mut().map(|ref mut state| state.added_res_header = res_header);
    self.front_buf_position = 0;
    self.back_buf_position = 0;
    self.front_buf.reset();
    self.back_buf.reset();
    self.readiness = Readiness::new();
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
    self.state.as_mut().unwrap()
  }

  pub fn set_state(&mut self, state: HttpState) {
    self.state = Some(state);
  }

  pub fn set_answer(&mut self, buf: &[u8])  {
    self.back_buf.reset();
    self.back_buf.write(buf);
    self.status = ClientStatus::DefaultAnswer;
  }

  //FIXME: maybe pregenerate it
  pub fn added_request_header(&self) -> String {
    use std::net::IpAddr;
    if let (Ok(peer), Ok(front)) = (
      self.front_socket().peer_addr().map(|addr| addr.ip()),
      self.front_socket().local_addr().map(|addr| addr.ip())
    ) {
      match (peer, front) {
        (IpAddr::V4(p), IpAddr::V4(f)) => format!("Forwarded: for={};by={}\r\nRequest-id: {}\r\n", peer, front, self.request_id),
        (IpAddr::V4(p), IpAddr::V6(f)) => format!("Forwarded: for={};by=\"{}\"\r\nRequest-id: {}\r\n", peer, front, self.request_id),
        (IpAddr::V6(p), IpAddr::V4(f)) => format!("Forwarded: for=\"{}\";by={}\r\nRequest-id: {}\r\n", peer, front, self.request_id),
        (IpAddr::V6(p), IpAddr::V6(f)) => format!("Forwarded: for=\"{}\";by=\"{}\"\r\nRequest-id: {}\r\n", peer, front, self.request_id),
      }
    } else {
      format!("Request-id: {}\r\n", self.request_id)
    }
  }

  //FIXME: maybe pregenerate it
  pub fn added_response_header(&self) -> String {
    format!("Request-id: {}\r\n", self.request_id)
  }
}

impl<Front:SocketHandler> ProxyClient for Client<Front> {
  fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  fn front_token(&self)  -> Option<Token> {
    self.token
  }

  fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t{}\t", self.server_context, self.request_id, app_id)
    } else {
      format!("{}\t{}\tunknown\t", self.server_context, self.request_id)
    }
  }

  fn front_timeout(&mut self) -> Option<Timeout> {
    self.front_timeout.take()
  }

  fn back_timeout(&mut self) -> Option<Timeout> {
    self.back_timeout.take()
  }

  fn set_front_timeout(&mut self, timeout: Timeout) {
    self.front_timeout = Some(timeout)
  }

  fn set_back_timeout(&mut self, timeout: Timeout) {
    self.back_timeout = Some(timeout)
  }

  fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend         = Some(socket);
  }

  fn set_front_token(&mut self, token: Token) {
    self.token         = Some(token);
  }

  fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn readiness(&mut self) -> &mut Readiness {
    &mut self.readiness
  }

  //FIXME: too much cloning in there, should optimize
  //FIXME: unwrap bad, bad rust coder
  fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.log_context(), self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize());
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  fn front_hup(&mut self) -> ClientResult {
    if self.backend_token == None {
      ClientResult::CloseClient
    } else {
      ClientResult::Continue
    }
  }

  fn back_hup(&mut self) -> ClientResult {
    if self.token == None {
      ClientResult::CloseClient
    } else {
      ClientResult::Continue
    }
  }

  // Read content from the client
  fn readable(&mut self) -> ClientResult {
    if self.status == ClientStatus::DefaultAnswer {
      self.readiness.front_interest.insert(EventSet::writable());
      self.readiness.back_interest.remove(EventSet::readable());
      self.readiness.back_interest.remove(EventSet::writable());
      return ClientResult::Continue;
    }

    //trace!("{}\treadable front pos: {}, buf pos: {}, available: {}", self.log_context(), self.state.req_position, self.front_buf_position, self.front_buf.buffer.available_data());
    assert!(!self.state.as_ref().unwrap().is_front_error());

    if self.front_buf.buffer.available_space() == 0 {
      if self.backend_token == None {
        // We don't have a backend to empty the buffer into, close the connection
        error!("{}\t[{:?}] front buffer full, no backend, closing the connection", self.log_context(), self.token);
        self.readiness.front_interest = EventSet::none();
        self.readiness.back_interest  = EventSet::none();
        return ClientResult::CloseClient;
      } else {
        self.readiness.front_interest.remove(EventSet::readable());
        self.readiness.back_interest.insert(EventSet::writable());
        return ClientResult::Continue;
      }
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.buffer.space());
    debug!("{}\tFRONT [{:?}]: read {} bytes", self.log_context(), self.token, sz);

    if sz > 0 {
      self.front_buf.buffer.fill(sz);
      self.front_buf.sliced_input(sz);

      if self.front_buf.start_parsing_position > self.front_buf.parsed_position {
        let to_consume = min(self.front_buf.input_data_size(),
        self.front_buf.start_parsing_position - self.front_buf.parsed_position);
        self.front_buf.consume_parsed_data(to_consume);
      }

    }

    if self.front_buf.buffer.available_space() == 0 {
      self.readiness.front_interest.remove(EventSet::readable());
    }

    if sz == 0 {
      self.readiness.front_readiness.remove(EventSet::readable());
    }

    match res {
      SocketResult::Error => {
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(EventSet::readable());
      },
      SocketResult::Continue => {}
    };

    let has_host = self.state.as_ref().unwrap().has_host();
    if !has_host {
      self.state = Some(parse_request_until_stop(self.state.take().unwrap(), &self.request_id,
        &mut self.front_buf));
      if self.state.as_ref().unwrap().is_front_error() {
        time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
        self.readiness.front_interest.remove(EventSet::readable());
        return ClientResult::CloseClient;
      }

      if self.state.as_ref().unwrap().has_host() {
        self.readiness.back_interest.insert(EventSet::writable());
        return ClientResult::ConnectBackend;
      } else {
        return ClientResult::Continue;
      }
    } else {
      self.readiness.back_interest.insert(EventSet::writable());
      match self.state.as_ref().unwrap().request {
        Some(RequestState::Request(_,_,_)) | Some(RequestState::RequestWithBody(_,_,_,_)) => {
          if ! self.front_buf.needs_input() {
            self.readiness.front_interest.remove(EventSet::readable());
            return  ClientResult::Continue;
          } else {
            return  ClientResult::Continue;
          }
        },
        Some(RequestState::RequestWithBodyChunks(_,_,_,ch)) => {
          if ch == Chunk::Ended {
            error!("{}\tfront read should have stopped on chunk ended", self.log_context(),);
            self.readiness.front_interest.remove(EventSet::readable());
            return ClientResult::Continue;
          } else if ch == Chunk::Error {
            error!("{}\tfront read should have stopped on chunk error", self.log_context(),);
            self.readiness.reset();
            return ClientResult::CloseClient;
          } else {
            //if self.front_buf_position + self.front_buf.buffer.available_data() >= self.state.req_position {
            if ! self.front_buf.needs_input() {
              self.state = Some(parse_request_until_stop(self.state.take().unwrap(), &self.request_id,
                &mut self.front_buf));
              //debug!("{}\tparse_request_until_stop returned {:?} => advance: {}", self.log_context(), self.state, self.state.req_position);
              if self.state.as_ref().unwrap().is_front_error() {
                time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                self.readiness.reset();
                return ClientResult::CloseClient;
              }

              if let Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) = self.state.as_ref().unwrap().request {
                self.readiness.front_interest.remove(EventSet::readable());
                return ClientResult::Continue;
              } else {
                return ClientResult::Continue;
              }
            } else {
              return ClientResult::Continue;
            }
          }
        },
      _ => {
          self.state = Some(parse_request_until_stop(self.state.take().unwrap(), &self.request_id,
            &mut self.front_buf));
          //debug!("{}\tparse_request_until_stop returned {:?} => advance: {}", self.log_context(), self.state, self.state.req_position);
          if self.state.as_ref().unwrap().is_front_error() {
            time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
            self.readiness.reset();
            return ClientResult::CloseClient;
          }

          if let Some(RequestState::Request(_,_,_)) = self.state.as_ref().unwrap().request {
            self.readiness.front_interest.remove(EventSet::readable());
            self.readiness.back_interest.insert(EventSet::writable());
            return ClientResult::Continue;
          } else {
            self.readiness.back_interest.insert(EventSet::writable());
            return ClientResult::Continue;
          }
        }
      }
    }
  }

  // Forward content to client
  fn writable(&mut self) -> ClientResult {

    let output_size = self.back_buf.output_data_size();
    if self.status == ClientStatus::DefaultAnswer {
      if self.back_buf.output_data_size() == 0 {
        self.readiness.front_interest.remove(EventSet::writable());
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
        self.readiness.front_readiness.remove(EventSet::writable());
      }

      if self.back_buf.buffer.available_data() == 0 {
        self.readiness.reset();
        println!("FIXME: should not close until the write ended");
        return ClientResult::CloseClient;
      }

      if res == SocketResult::Error {
        self.readiness.reset();
        return ClientResult::CloseClient;
      } else {
        return ClientResult::Continue;
      }
    }

    if self.back_buf.output_data_size() == 0 {
      self.readiness.back_interest.insert(EventSet::readable());
      self.readiness.front_interest.remove(EventSet::writable());
      return ClientResult::Continue;
    }

    let mut sz = 0usize;
    let mut res = SocketResult::Continue;
    while res == SocketResult::Continue && self.back_buf.output_data_size() > 0 {
      let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.next_output_data());
      res = current_res;
      //println!("FRONT_WRITABLE[{}] wrote {} bytes:\n{}\nres={:?}", line!(), sz, self.back_buf.next_output_data().to_hex(16), res);
      self.back_buf.consume_output_data(current_sz);
      self.back_buf_position += current_sz;
      sz += current_sz;
    }

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}, buffer position {} restart position {}", self.log_context(), front.as_usize(), back.as_usize(), sz, output_size, self.back_buf.buffer_position, self.back_buf.start_parsing_position);
      //debug!("{}\tFRONT [{}<-{}]: back buf: {:?}", self.log_context(), front.as_usize(), back.as_usize(), *self.back_buf);
    }

    match res {
      SocketResult::Error => {
        self.readiness.reset();
        return ClientResult::CloseClient;
      },
      SocketResult::WouldBlock => {
        self.readiness.front_readiness.remove(EventSet::writable());
      },
      SocketResult::Continue => {},
    }

    if self.back_buf.can_restart_parsing() {
      match self.state.as_ref().unwrap().response {
        // FIXME: should only restart parsing if we are using keepalive
        Some(ResponseState::Response(_,_))                            |
          Some(ResponseState::ResponseWithBody(_,_,_))                  |
          Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
            self.reset();
            self.readiness.front_interest.insert(EventSet::readable());
            ClientResult::Continue
          },
          // restart parsing, since there will be other chunks next
          Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
            self.readiness.back_interest.insert(EventSet::readable());
            ClientResult::Continue
          },
          _ => {
            self.readiness.reset();
            ClientResult::CloseBothFailure
          }
      }
    } else {
      self.readiness.back_interest.insert(EventSet::readable());
      ClientResult::Continue
    }
  }

  // Forward content to application
  fn back_writable(&mut self) -> ClientResult {
    if self.status == ClientStatus::DefaultAnswer {
      self.readiness.back_interest.remove(EventSet::writable());
      self.readiness.front_interest.insert(EventSet::writable());
      return ClientResult::Continue;
    }

    //trace!("{}\twritable back pos: {}, buf pos: {}, available: {}", self.log_context(), self.state.req_position, self.front_buf_position, self.front_buf.buffer.available_data());
    //assert!(self.front_buf_position + self.front_buf.available_data() <= self.state.req_position);
    if self.front_buf.output_data_size() == 0 {
      self.readiness.front_interest.insert(EventSet::readable());
      self.readiness.back_interest.remove(EventSet::writable());
      return ClientResult::Continue;
    }

    let tokens = self.tokens().clone();
    let context = self.log_context();
    let output_size = self.front_buf.output_data_size();
    let res = if let Some(ref mut sock) = self.backend {
      //let (sz, socket_res) = sock.socket_write(&(self.front_buf.next_buffer_unwrap())[..to_copy]);
      let mut sz = 0usize;
      let mut socket_res = SocketResult::Continue;

      while socket_res == SocketResult::Continue && self.front_buf.output_data_size() > 0 {
        let (current_sz, current_res) = sock.socket_write(self.front_buf.next_output_data());
        socket_res = current_res;
        //println!("BACK_WRITABLE[{}] wrote {} bytes:\n{}\nres={:?}", line!(), current_sz, self.front_buf.next_output_data().to_hex(16), socket_res);
        self.front_buf.consume_output_data(current_sz);
        self.front_buf_position += current_sz;
        sz += current_sz;
      }

      if let Some((front,back)) = tokens {
        debug!("{}\tBACK [{}->{}]: wrote {} bytes of {}", context, front.as_usize(), back.as_usize(), sz, output_size);
      }
      match socket_res {
        SocketResult::Error => {
          self.readiness.reset();
          return ClientResult::CloseBothFailure;
        },
        SocketResult::WouldBlock => {
          self.readiness.back_readiness.remove(EventSet::writable());

        },
        SocketResult::Continue => {}
      }

      // FIXME/ should read exactly as much data as needed
      //if self.front_buf_position >= self.state.req_position {
      if self.front_buf.can_restart_parsing() {
        match self.state.as_ref().unwrap().request {
          Some(RequestState::Request(_,_,_))                            |
          Some(RequestState::RequestWithBody(_,_,_,_))                  |
          Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
            self.readiness.front_interest.remove(EventSet::readable());
            self.readiness.back_interest.insert(EventSet::readable());
            self.readiness.back_interest.remove(EventSet::writable());
            ClientResult::Continue
          },
          Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
            self.readiness.front_interest.insert(EventSet::readable());
            ClientResult::Continue
          },
          _ => {
            self.readiness.reset();
            ClientResult::CloseBothFailure
          }
        }
      } else {
        self.readiness.front_interest.insert(EventSet::readable());
        self.readiness.back_interest.insert(EventSet::writable());
        ClientResult::Continue
      }
    } else {
      self.readiness.reset();
      return ClientResult::CloseBothFailure;
    };

    res
  }

  // Read content from application
  fn back_readable(&mut self) -> ClientResult {
    if self.status == ClientStatus::DefaultAnswer {
      self.readiness.back_interest.remove(EventSet::readable());
      return ClientResult::Continue;
    }

    //trace!("{}\treadable back pos: {}, buf pos: {}, available: {}", self.log_context(), self.state.res_position, self.back_buf_position, self.back_buf.buffer.available_data());
    //assert!(self.back_buf_position + self.back_buf.available_data() <= self.state.res_position);

    if self.back_buf.buffer.available_space() == 0 {
      //println!("BACK BUFFER FULL({} bytes): TOKENS {:?} {:?}", self.back_buf.available_data(), self.token, self.backend_token);
      self.readiness.back_interest.remove(EventSet::readable());
      return ClientResult::Continue;
    }

    let tokens     = self.tokens().clone();
    let context    = self.log_context();

    if let Some(ref mut sock) = self.backend {
      let (sz, r) = sock.socket_read(&mut self.back_buf.buffer.space());
      self.back_buf.buffer.fill(sz);
      self.back_buf.sliced_input(sz);
      //println!("BACK_READABLE[{}]\ndata:\n{}unparsed data:\n{}", line!(), self.back_buf.buffer.data().to_hex(16), self.back_buf.unparsed_data().to_hex(16));
      if let Some((front,back)) = tokens {
        debug!("{}\tBACK  [{}<-{}]: read {} bytes", context, front.as_usize(), back.as_usize(), sz);
      }

      if r != SocketResult::Continue || sz == 0 {
        self.readiness.back_readiness.remove(EventSet::readable());
      }

      match r {
        SocketResult::Error => {
          self.readiness.reset();
          ClientResult::CloseBothFailure
        },
        _                   => {
          match self.state.as_ref().unwrap().response {
            Some(ResponseState::Response(_,_)) => {
              //FIXME: this keeps happening, why? Readable event already in queue when parsing ended?
              error!("{}\tshould not go back in back_readable if the whole response was parsed", context);
              self.readiness.back_interest.remove(EventSet::readable());
              return  ClientResult::Continue;
            },
            Some(ResponseState::ResponseWithBody(_,_,_)) => {
              self.readiness.front_interest.insert(EventSet::writable());
              if ! self.back_buf.needs_input() {
                self.readiness.back_interest.remove(EventSet::readable());
                return ClientResult::Continue;
              } else {
                return ClientResult::Continue;
              }
            },
            Some(ResponseState::ResponseWithBodyChunks(_,_,ch)) => {
              if ch == Chunk::Ended {
                error!("{}\tback read should have stopped on chunk ended", context);
                self.readiness.back_interest.remove(EventSet::readable());
                return ClientResult::Continue;
              } else if ch == Chunk::Error {
                error!("{}\tback read should have stopped on chunk error", context);
                self.readiness.reset();
                return ClientResult::CloseClient;
              } else {
                //if self.back_buf_position + self.back_buf.buffer.available_data() >= self.state.res_position {
                if ! self.back_buf.needs_input() {
                  self.state = Some(parse_response_until_stop(self.state.take().unwrap(), &self.request_id,
                    &mut self.back_buf));
                  //debug!("{}\tparse_response_until_stop returned {:?} => advance: {}", context, self.state, self.state.res_position);
                  if self.state.as_ref().unwrap().is_back_error() {
                    time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                    self.readiness.reset();
                    return ClientResult::CloseBothFailure;
                  }

                  if let Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) = self.state.as_ref().unwrap().response {
                    self.readiness.back_interest.remove(EventSet::readable());
                    return ClientResult::Continue;
                  } else {
                    self.readiness.front_interest.insert(EventSet::writable());
                    return ClientResult::Continue;
                  }
                } else {
                  return ClientResult::Continue;
                }
              }
            },
            Some(ResponseState::Error(_)) => panic!("{}\tback read should have stopped on responsestate error", context),
            _ => {
              self.state = Some(parse_response_until_stop(self.state.take().unwrap(), &self.request_id,
                &mut self.back_buf));
              //debug!("{}\tparse_response_until_stop returned {:?} => advance: {}", context, self.state, self.state.res_position);
              if self.state.as_ref().unwrap().is_back_error() {
                time!("http_proxy.failure", (precise_time_ns() - self.start) / 1000);
                self.readiness.reset();
                return ClientResult::CloseBothFailure;
              }

              if let Some(ResponseState::Response(_,_)) = self.state.as_ref().unwrap().response {
                self.readiness.front_interest.insert(EventSet::writable());
                self.readiness.back_interest.remove(EventSet::readable());
                return ClientResult::Continue;
              } else {
                self.readiness.front_interest.insert(EventSet::writable());
                return ClientResult::Continue;
              }
            }
          }
        }
      }
    } else {
      self.readiness.reset();
      return ClientResult::CloseBothFailure;
    }
  }

}

type ClientToken = Token;

#[allow(non_snake_case)]
pub struct DefaultAnswers {
  pub NotFound:           Vec<u8>,
  pub ServiceUnavailable: Vec<u8>
}

pub type AppId    = String;
pub type Hostname = String;

pub struct ServerConfiguration {
  listener:  TcpListener,
  address:   SocketAddr,
  instances: HashMap<AppId, Vec<Backend>>,
  fronts:    HashMap<Hostname, Vec<HttpFront>>,
  tx:        mpsc::Sender<ServerMessage>,
  pool:      Pool<BufferQueue>,
  answers:   DefaultAnswers,
  front_timeout:   u64,
  back_timeout:    u64,
}

impl ServerConfiguration {
  pub fn new(address: SocketAddr, tx: mpsc::Sender<ServerMessage>, max_connections: usize, buffer_size: usize, event_loop: &mut EventLoop<HttpServer>) -> io::Result<ServerConfiguration> {
    match server_bind(&address) {
      Ok(sock) => {
        event_loop.register(&sock, Token(0), EventSet::readable(), PollOpt::level());
        Ok(ServerConfiguration {
          listener:  sock,
          address:   address,
          instances: HashMap::new(),
          fronts:    HashMap::new(),
          tx:        tx,
          pool:      Pool::with_capacity(2*max_connections, 0, || BufferQueue::with_capacity(buffer_size)),
          //FIXME: make the timeout values configurable
          front_timeout: 5000,
          back_timeout:  5000,
          answers:   DefaultAnswers {
            NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
            ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]),
          }
        })
      },
      Err(e) => {
        error!("HTTP\tcould not create listener {:?}: {:?}", address, e);
        Err(e)
      }
    }
  }

  pub fn add_http_front(&mut self, http_front: HttpFront, event_loop: &mut EventLoop<HttpServer>) {
    let front2 = http_front.clone();
    let front3 = http_front.clone();
    if let Some(fronts) = self.fronts.get_mut(&http_front.hostname) {
        fronts.push(front2);
    }

    // FIXME: check that http front port matches the listener's port

    if self.fronts.get(&http_front.hostname).is_none() {
      self.fronts.insert(http_front.hostname, vec![front3]);
    }
  }

  pub fn remove_http_front(&mut self, front: HttpFront, event_loop: &mut EventLoop<HttpServer>) {
    info!("HTTP\tremoving http_front {:?}", front);
    if let Some(fronts) = self.fronts.get_mut(&front.hostname) {
      fronts.retain(|f| f != &front);
    }
  }

  pub fn add_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<HttpServer>) {
    if let Some(addrs) = self.instances.get_mut(app_id) {
      let backend = Backend::new(*instance_address);
      addrs.push(backend);
    }

    if self.instances.get(app_id).is_none() {
      let backend = Backend::new(*instance_address);
      self.instances.insert(String::from(app_id), vec![backend]);
    }
  }

  pub fn remove_instance(&mut self, app_id: &str, instance_address: &SocketAddr, event_loop: &mut EventLoop<HttpServer>) {
      if let Some(instances) = self.instances.get_mut(app_id) {
        instances.retain(|backend| &backend.address != instance_address);
      } else {
        error!("HTTP\tInstance was already removed");
      }
  }

  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&HttpFront> {
    if let Some(http_fronts) = self.fronts.get(host) {
      let matching_fronts = http_fronts.iter().filter(|f| uri.starts_with(&f.path_begin)); // ToDo match on uri
      let mut front = None;

      for f in matching_fronts {
        if front.is_none() {
          front = Some(f);
        }

        if let Some(ff) = front {
          if f.path_begin.len() > ff.path_begin.len() {
            front = Some(f)
          }
        }
      }
      front
    } else {
      None
    }
  }

  pub fn backend_from_request(&mut self, client: &mut Client<TcpStream>, host: &str, uri: &str) -> Result<TcpStream,ConnectionError> {
    // FIXME: the app id clone here is probably very inefficient
    if let Some(app_id) = self.frontend_from_request(host, uri).map(|ref front| front.app_id.clone()) {
      client.app_id = Some(app_id.clone());
      // ToDo round-robin on instances
      if let Some(ref mut app_instances) = self.instances.get_mut(&app_id) {
        if app_instances.len() == 0 {
          client.set_answer(&self.answers.ServiceUnavailable);
          return Err(ConnectionError::NoBackendAvailable);
        }
        let rnd = random::<usize>();
        let mut instances:Vec<&mut Backend> = app_instances.iter_mut().filter(|backend| backend.can_open()).collect();
        let idx = rnd % instances.len();
        info!("{}\tConnecting {} -> {:?}", client.log_context(), host, instances.get(idx).map(|backend| (backend.address, backend.active_connections)));
        instances.get_mut(idx).ok_or(ConnectionError::NoBackendAvailable).and_then(|ref mut backend| {
          let conn: Result<TcpStream, ConnectionError> = TcpStream::connect(&backend.address).map_err(|_| ConnectionError::NoBackendAvailable);
          if conn.is_ok() {
            backend.inc_connections();
          }
          conn
        })
      } else {
        Err(ConnectionError::NoBackendAvailable)
      }
    } else {
      Err(ConnectionError::HostNotFound)
    }
  }

}

impl ProxyConfiguration<HttpServer,Client<TcpStream>> for ServerConfiguration {
  fn connect_to_backend(&mut self, client: &mut Client<TcpStream>) -> Result<(),ConnectionError> {
      let host   = try!(client.state.as_ref().unwrap().get_host().ok_or(ConnectionError::NoHostGiven));
      let rl     = try!(client.state.as_ref().unwrap().get_request_line().ok_or(ConnectionError::NoRequestLineGiven));
      let conn   = self.backend_from_request(client, &host, &rl.uri);

      match conn {
        Ok(socket) => {
        socket.set_nodelay(true);
        client.set_back_socket(socket);
        client.readiness().back_interest.insert(EventSet::writable());
        Ok(())
        },
        Err(ConnectionError::NoBackendAvailable) => {
          client.set_answer(&self.answers.ServiceUnavailable);
          client.readiness().front_interest.insert(EventSet::writable());
          Err(ConnectionError::NoBackendAvailable)
        }
        Err(ConnectionError::HostNotFound) => {
          client.set_answer(&self.answers.NotFound);
          client.readiness().front_interest.insert(EventSet::writable());
          Err(ConnectionError::HostNotFound)
        }
        e => panic!(e)
      }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<HttpServer>, message: ProxyOrder) {
  // ToDo temporary
    trace!("HTTP\t{} notified", message);
    match message {
      ProxyOrder::Command(id, Command::AddHttpFront(front)) => {
        info!("HTTP\t{} add front {:?}", id, front);
          self.add_http_front(front, event_loop);
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::AddedFront});
      },
      ProxyOrder::Command(id, Command::RemoveHttpFront(front)) => {
        info!("HTTP\t{} front {:?}", id, front);
        self.remove_http_front(front, event_loop);
        self.tx.send(ServerMessage{ id: id, message: ServerMessageType::RemovedFront});
      },
      ProxyOrder::Command(id, Command::AddInstance(instance)) => {
        info!("HTTP\t{} add instance {:?}", id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.add_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::AddedInstance});
        } else {
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::RemoveInstance(instance)) => {
        info!("HTTP\t{} remove instance {:?}", id, instance);
        let addr_string = instance.ip_address + ":" + &instance.port.to_string();
        let parsed:Option<SocketAddr> = addr_string.parse().ok();
        if let Some(addr) = parsed {
          self.remove_instance(&instance.app_id, &addr, event_loop);
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::RemovedInstance});
        } else {
          self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("cannot parse the address"))});
        }
      },
      ProxyOrder::Command(id, Command::HttpProxy(configuration)) => {
        info!("HTTP\t{} modifying proxy configuration: {:?}", id, configuration);
        self.front_timeout = configuration.front_timeout;
        self.back_timeout  = configuration.back_timeout;
        self.answers = DefaultAnswers {
          NotFound:           configuration.answer_404.into_bytes(),
          ServiceUnavailable: configuration.answer_503.into_bytes(),
        };
      },
      ProxyOrder::Stop(id)                   => {
        info!("HTTP\t{} shutdown", id);
        event_loop.shutdown();
        self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Stopped});
      },
      ProxyOrder::Command(id, msg) => {
        debug!("HTTP\t{} unsupported message, ignoring: {:?}", id, msg);
        self.tx.send(ServerMessage{ id: id, message: ServerMessageType::Error(String::from("unsupported message"))});
      }
    }
  }

  fn accept(&mut self, token: Token) -> Option<(Client<TcpStream>, bool)> {
    if let (Some(front_buf), Some(back_buf)) = (self.pool.checkout(), self.pool.checkout()) {
      let accepted = self.listener.accept();

      if let Ok(Some((frontend_sock, _))) = accepted {
        frontend_sock.set_nodelay(true);
        if let Some(mut c) = Client::new("HTTP", frontend_sock, front_buf, back_buf) {
          c.readiness().front_interest.insert(EventSet::readable());
          c.readiness().back_interest.remove(EventSet::readable() | EventSet::writable());
          return Some((c, false))
        }
      } else {
        error!("HTTP\tcould not accept: {:?}", accepted);
      }
    } else {
      error!("HTTP\tcould not get buffers from pool");
    }
    None
  }

  fn close_backend(&mut self, app_id: String, addr: &SocketAddr) {
    if let Some(app_instances) = self.instances.get_mut(&app_id) {
      if let Some(ref mut backend) = app_instances.iter_mut().find(|backend| &backend.address == addr) {
        backend.dec_connections();
      }
    }
  }

  fn front_timeout(&self) -> u64 {
    self.front_timeout
  }

  fn back_timeout(&self)  -> u64 {
    self.back_timeout
  }
}

pub type HttpServer = Server<ServerConfiguration,Client<TcpStream>>;

pub fn start_listener(front: SocketAddr, max_connections: usize, buffer_size: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<ProxyOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();

  let configuration = ServerConfiguration::new(front, tx, max_connections, buffer_size, &mut event_loop).unwrap();
  let mut server = HttpServer::new(1, max_connections, configuration);

  let join_guard = thread::spawn(move|| {
    info!("HTTP\tstarting event loop");
    event_loop.run(&mut server).unwrap();
    info!("HTTP\tending event loop");
    //notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}

#[cfg(test)]
mod tests {
  extern crate tiny_http;
  use super::*;
  use mio::util::Slab;
  use std::collections::HashMap;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};
  use std::sync::mpsc::channel;
  use std::net::SocketAddr;
  use std::str::FromStr;
  use std::time::Duration;
  use messages::{Command,HttpFront,Instance};
  use network::{ProxyOrder,ServerMessage};
  use network::buffer_queue::BufferQueue;
  use pool::Pool;

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    start_server(1025);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1024").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 12000, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1024"), path_begin: String::from("/") };
    sender.send(ProxyOrder::Command(String::from("ID_ABCD"), Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1025 };
    sender.send(ProxyOrder::Command(String::from("ID_EFGH"), Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1024)).unwrap();
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep(Duration::from_millis(100));
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1024\r\nConnection: Close\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep(Duration::from_millis(500));
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

        //thread::sleep(Duration::from_millis(300));
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 204);
        //assert!(false);
      }
    }
  }

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn keep_alive() {
    start_server(1028);
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1031").unwrap();
    let (tx,rx) = channel::<ServerMessage>();
    let (sender, jg) = start_listener(front, 10, 12000, tx.clone());
    let front = HttpFront { app_id: String::from("app_1"), hostname: String::from("localhost:1031"), path_begin: String::from("/") };
    sender.send(ProxyOrder::Command(String::from("ID_ABCD"), Command::AddHttpFront(front)));
    let instance = Instance { app_id: String::from("app_1"), ip_address: String::from("127.0.0.1"), port: 1028 };
    sender.send(ProxyOrder::Command(String::from("ID_EFGH"), Command::AddInstance(instance)));
    println!("test received: {:?}", rx.recv());
    println!("test received: {:?}", rx.recv());
    thread::sleep(Duration::from_millis(300));

    let mut client = TcpStream::connect(("127.0.0.1", 1031)).unwrap();
    // 5 seconds of timeout
    client.set_read_timeout(Some(Duration::new(5,0)));
    thread::sleep(Duration::from_millis(100));
    let mut w  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
    println!("http client write: {:?}", w);
    let mut buffer = [0;4096];
    thread::sleep(Duration::from_millis(500));
    let mut r = client.read(&mut buffer[..]);
    println!("http client read: {:?}", r);
    match r {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer[..]).unwrap());

        //thread::sleep(Duration::from_millis(300));
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 204);
        //assert!(false);
      }
    }

    println!("first request ended, will send second one");
    let mut buffer2 = [0;4096];
    let mut w2  = client.write(&b"GET / HTTP/1.1\r\nHost: localhost:1031\r\n\r\n"[..]);
    println!("http client write: {:?}", w2);
    thread::sleep(Duration::from_millis(500));
    let mut r2 = client.read(&mut buffer2[..]);
    println!("http client read: {:?}", r2);
    match r2 {
      Err(e)      => assert!(false, "client request should not fail. Error: {:?}",e),
      Ok(sz) => {
        // Read the Response.
        println!("read response");

        println!("Response: {}", str::from_utf8(&buffer2[..]).unwrap());

        //thread::sleep(Duration::from_millis(300));
        //assert_eq!(&body, &"Hello World!"[..]);
        assert_eq!(sz, 204);
        //assert!(false);
      }
    }
  }


  use self::tiny_http::{ServerBuilder, Response};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server(port: u16) {
    thread::spawn(move|| {
      let server = ServerBuilder::new().with_port(port).build().unwrap();
      println!("starting web server in port {}", port);

      for request in server.incoming_requests() {
        println!("backend web server got request -> method: {:?}, url: {:?}, headers: {:?}",
          request.method(),
          request.url(),
          request.headers()
        );

        let response = Response::from_string("hello world");
        request.respond(response);
        println!("backend web server sent response");
      }

      println!("server on port {} closed", port);
    });
  }

  use mio::tcp;
  #[test]
  fn frontend_from_request_test() {
    let app_id1 = "app_1".to_owned();
    let app_id2 = "app_2".to_owned();
    let app_id3 = "app_3".to_owned();
    let uri1 = "/".to_owned();
    let uri2 = "/yolo".to_owned();
    let uri3 = "/yolo/swag".to_owned();

    let mut fronts = HashMap::new();
    fronts.insert("lolcatho.st".to_owned(), vec![
      HttpFront { app_id: app_id1, hostname: "lolcatho.st".to_owned(), path_begin: uri1 },
      HttpFront { app_id: app_id2, hostname: "lolcatho.st".to_owned(), path_begin: uri2 },
      HttpFront { app_id: app_id3, hostname: "lolcatho.st".to_owned(), path_begin: uri3 }
    ]);
    fronts.insert("other.domain".to_owned(), vec![
      HttpFront { app_id: "app_1".to_owned(), hostname: "other.domain".to_owned(), path_begin: "/test".to_owned() },
    ]);

    let (tx,rx) = channel::<ServerMessage>();

    let front: SocketAddr = FromStr::from_str("127.0.0.1:1030").unwrap();
    let listener = tcp::TcpListener::bind(&front).unwrap();
    let server_config = ServerConfiguration {
      listener:  listener,
      address:   front,
      instances: HashMap::new(),
      fronts:    fronts,
      tx:        tx,
      pool:      Pool::with_capacity(1,0, || BufferQueue::with_capacity(12000)),
      front_timeout: 50000,
      back_timeout:  50000,
      answers:   DefaultAnswers {
        NotFound: Vec::from(&b"HTTP/1.1 404 Not Found\r\n\r\n"[..]),
        ServiceUnavailable: Vec::from(&b"HTTP/1.1 503 your application is in deployment\r\n\r\n"[..]),
      }
    };

    let frontend1 = server_config.frontend_from_request("lolcatho.st", "/");
    let frontend2 = server_config.frontend_from_request("lolcatho.st", "/test");
    let frontend3 = server_config.frontend_from_request("lolcatho.st", "/yolo/test");
    let frontend4 = server_config.frontend_from_request("lolcatho.st", "/yolo/swag");
    let frontend5 = server_config.frontend_from_request("domain", "/");
    assert_eq!(frontend1.unwrap().app_id, "app_1");
    assert_eq!(frontend2.unwrap().app_id, "app_1");
    assert_eq!(frontend3.unwrap().app_id, "app_2");
    assert_eq!(frontend4.unwrap().app_id, "app_3");
    assert_eq!(frontend5, None);
  }
}
