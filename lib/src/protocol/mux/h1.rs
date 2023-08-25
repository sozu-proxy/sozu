use sozu_command::ready::Ready;

use crate::{
    protocol::mux::{Context, GlobalStreamId, MuxResult, Position},
    socket::{SocketHandler, SocketResult},
    Readiness,
};

use super::UpdateReadiness;

pub struct ConnectionH1<Front: SocketHandler> {
    pub position: Position,
    pub readiness: Readiness,
    pub socket: Front,
    /// note: a Server H1 will always reference stream 0, but a client can reference any stream
    pub stream: GlobalStreamId,
}

impl<Front: SocketHandler> ConnectionH1<Front> {
    pub fn readable<E>(&mut self, context: &mut Context, mut endpoint: E) -> MuxResult
    where
        E: UpdateReadiness,
    {
        println!("======= MUX H1 READABLE");
        let stream = &mut context.streams[self.stream];
        let kawa = stream.rbuffer(self.position);
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        println!("  size: {size}, status: {status:?}");
        if size > 0 {
            kawa.storage.fill(size);
        } else {
            self.readiness.event.remove(Ready::READABLE);
        }
        match status {
            SocketResult::Continue => {}
            SocketResult::Closed => todo!(),
            SocketResult::Error => todo!(),
            SocketResult::WouldBlock => self.readiness.event.remove(Ready::READABLE),
        }
        kawa::h1::parse(kawa, &mut kawa::h1::NoCallbacks);
        kawa::debug_kawa(kawa);
        if kawa.is_error() {
            return MuxResult::Close(self.stream);
        }
        if kawa.is_terminated() {
            self.readiness.interest.remove(Ready::READABLE);
        }
        if kawa.is_main_phase() {
            endpoint
                .readiness_mut(stream.token.unwrap())
                .interest
                .insert(Ready::WRITABLE)
        }
        MuxResult::Continue
    }
    pub fn writable<E>(&mut self, context: &mut Context, endpoint: E) -> MuxResult
    where
        E: UpdateReadiness,
    {
        println!("======= MUX H1 WRITABLE");
        let stream = &mut context.streams[self.stream];
        let kawa = stream.wbuffer(self.position);
        kawa.prepare(&mut kawa::h1::BlockConverter);
        kawa::debug_kawa(kawa);
        let bufs = kawa.as_io_slice();
        if bufs.is_empty() {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let (size, status) = self.socket.socket_write_vectored(&bufs);
        println!("  size: {size}, status: {status:?}");
        if size > 0 {
            kawa.consume(size);
            // self.backend_readiness.interest.insert(Ready::READABLE);
        } else {
            self.readiness.event.remove(Ready::WRITABLE);
        }
        if kawa.is_terminated() && kawa.is_completed() {
            self.readiness.interest.insert(Ready::READABLE);
        }
        MuxResult::Continue
    }
}
