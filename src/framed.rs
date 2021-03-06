#![allow(dead_code)]

use std;
use std::collections::VecDeque;

use futures::{Async, AsyncSink, Future, Poll, Sink, Stream};
use futures::sync::oneshot::Sender as SyncSender;
use futures::unsync::oneshot::{channel, Sender as UnsyncSender, Receiver as UnsyncReceiver};
use tokio_core::reactor::Handle;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::{Framed, Encoder};

use fut::ActorFuture;
use queue::unsync;

use actor::{Actor, Supervised, SpawnHandle,
            FramedActor, ActorState, ActorContext, AsyncContext};
use address::{Address, SyncAddress, Subscriber};
use handler::{Handler, ResponseType};
use context::AsyncContextApi;
use contextcells::{ContextCell, ContextCellResult, ContextProtocol};
use envelope::{Envelope, ToEnvelope, RemoteEnvelope};
use contextimpl::ContextImpl;


/// Actor execution context for
/// [Framed](https://docs.rs/tokio-io/0.1.3/tokio_io/codec/struct.Framed.html) object
pub struct FramedContext<A>
    where A: FramedActor + Actor<Context=FramedContext<A>>,
{
    inner: ContextImpl<A, ActorFramedCell<A>>,
}

impl<A> ToEnvelope<A> for FramedContext<A>
    where A: FramedActor + Actor<Context=FramedContext<A>>,
{
    #[inline]
    fn pack<M>(msg: M,
               tx: Option<SyncSender<Result<M::Item, M::Error>>>,
               cancel_on_drop: bool) -> Envelope<A>
        where M: ResponseType + Send + 'static,
              A: Handler<M> + FramedActor + Actor<Context=FramedContext<A>>,
              M::Item: Send, M::Error: Send {
        Envelope::new(RemoteEnvelope::new(msg, tx, cancel_on_drop))
    }
}

impl<A> ActorContext for FramedContext<A>
    where A: Actor<Context=Self> + FramedActor,
{
    #[inline]
    fn stop(&mut self) {
        self.inner.stop()
    }
    #[inline]
    fn terminate(&mut self) {
        self.inner.terminate()
    }
    #[inline]
    fn state(&self) -> ActorState {
        self.inner.state()
    }
}

impl<A> AsyncContext<A> for FramedContext<A>
    where A: Actor<Context=Self> + FramedActor,
{
    #[inline]
    fn spawn<F>(&mut self, fut: F) -> SpawnHandle
        where F: ActorFuture<Item=(), Error=(), Actor=A> + 'static
    {
        self.inner.spawn(fut)
    }

    #[inline]
    fn wait<F>(&mut self, fut: F)
        where F: ActorFuture<Item=(), Error=(), Actor=A> + 'static
    {
        self.inner.wait(fut)
    }

    #[inline]
    fn cancel_future(&mut self, handle: SpawnHandle) -> bool {
        self.inner.cancel_future(handle)
    }
}

impl<A> AsyncContextApi<A> for FramedContext<A>
    where A: Actor<Context=Self> + FramedActor,
{
    #[inline]
    fn unsync_sender(&mut self) -> unsync::UnboundedSender<ContextProtocol<A>> {
        self.inner.unsync_sender()
    }

    #[inline]
    fn unsync_address(&mut self) -> Address<A> {
        self.inner.unsync_address()
    }

    #[inline]
    fn sync_address(&mut self) -> SyncAddress<A> {
        self.inner.sync_address()
    }
}

impl<A> FramedContext<A> where A: Actor<Context=Self> + FramedActor
{
    /// Send item to sink. If sink is closed item returned as an error.
    #[inline]
    pub fn send(&mut self, msg: <A::Codec as Encoder>::Item)
                -> Result<(), <A::Codec as Encoder>::Item>
    {
        if let Some(ref mut framed) = *self.inner.cell() {
            framed.send(msg);
            Ok(())
        } else {
            Err(msg)
        }
    }

    /// Gracefully close Framed object. FramedContext
    /// will try to send all buffered items and then close.
    /// FramedContext::stop() could be used to force stop sending process.
    #[inline]
    pub fn close(&mut self) {
        if let Some(ref mut framed) = *self.inner.cell() {
            framed.close();
        }
    }

    /// Initiate sink drain
    ///
    /// Returns oneshot future. It resolves when sink is drained.
    /// All other actor activities are paused.
    #[inline]
    pub fn drain(&mut self) -> UnsyncReceiver<()> {
        if let Some(ref mut framed) = *self.inner.cell() {
            framed.drain()
        } else {
            let (tx, rx) = channel();
            let _ = tx.send(());
            rx
        }
    }

    /// Get inner framed object
    #[inline]
    pub fn take(&mut self) -> Option<Framed<A::Io, A::Codec>> {
        if let Some(cell) = self.inner.cell().take() {
            Some(cell.into_framed())
        } else {
            None
        }
    }

    /// Replace existing framed object with new object.
    ///
    /// Consider to use `drain()` before replace framed object,
    /// because Sink buffer get dropped as well.
    pub fn replace(&mut self, framed: Framed<A::Io, A::Codec>) {
        self.inner.modify();
        if let Some(ref mut cell) = *self.inner.cell() {
            cell.replace(framed);
            return
        }
        *self.inner.cell() = Some(ActorFramedCell::new(framed));
    }
}

#[doc(hidden)]
impl<A> FramedContext<A> where A: Actor<Context=Self> + FramedActor
{
    #[inline]
    pub fn subscriber<M>(&mut self) -> Box<Subscriber<M>>
        where A: Handler<M>, M: ResponseType + 'static
    {
        self.inner.subscriber()
    }

    #[inline]
    pub fn sync_subscriber<M>(&mut self) -> Box<Subscriber<M> + Send>
        where A: Handler<M>,
              M: ResponseType + Send + 'static,
              M::Item: Send, M::Error: Send {
        self.inner.sync_subscriber()
    }
}

impl<A> FramedContext<A> where A: Actor<Context=Self> + FramedActor
{
    #[inline]
    pub(crate) fn new(act: Option<A>, io: A::Io, codec: A::Codec) -> FramedContext<A> {
        FramedContext {
            inner: ContextImpl::with_cell(
                act, ActorFramedCell::new(io.framed(codec)))
        }
    }

    #[inline]
    pub(crate) fn framed(act: Option<A>, framed: Framed<A::Io, A::Codec>) -> FramedContext<A> {
        FramedContext {
            inner: ContextImpl::with_cell(act, ActorFramedCell::new(framed))
        }
    }

    #[inline]
    pub(crate) fn run(self, handle: &Handle) {
        handle.spawn(self.map(|_| ()).map_err(|_| ()));
    }

    #[inline]
    pub(crate) fn restarting(&mut self) where A: Supervised {
        let ctx: &mut FramedContext<A> = unsafe {
            std::mem::transmute(self as &mut FramedContext<A>)
        };
        self.inner.actor().restarting(ctx);
    }

    #[inline]
    pub(crate) fn set_actor(&mut self, act: A) {
        self.inner.set_actor(act)
    }

    #[inline]
    pub(crate) fn into_inner(self) -> A {
        self.inner.into_inner().unwrap()
    }
}

#[doc(hidden)]
impl<A> Future for FramedContext<A> where A: Actor<Context=Self> + FramedActor
{
    type Item = ();
    type Error = ();

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let ctx: &mut FramedContext<A> = unsafe {
            std::mem::transmute(self as &mut FramedContext<A>)
        };
        self.inner.poll(ctx)
    }
}

impl<A> std::fmt::Debug for FramedContext<A>
    where A: Actor<Context=Self> + FramedActor,
{
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "FramedContext({:?})", self as *const _)
    }
}

bitflags! {
    struct FramedFlags: u8 {
        const STARTED = 0b0000_0001;
        const CLOSING = 0b0000_0010;
        const STREAM_CLOSED = 0b0000_0100;
        const SINK_CLOSED = 0b0000_1000;
        const SINK_FLUSHED = 0b0001_0000;
    }
}

/// Framed object wrapper
pub(crate)
struct ActorFramedCell<A>
    where A: Actor + FramedActor,
          A::Context: AsyncContext<A>,
{
    flags: FramedFlags,
    framed: Framed<A::Io, A::Codec>,
    sink_items: VecDeque<<A::Codec as Encoder>::Item>,
    drain: Option<UnsyncSender<()>>,
    error: Option<<A::Codec as Encoder>::Error>,
}

impl<A> ActorFramedCell<A>
    where A: Actor + FramedActor, A::Context: AsyncContext<A>
{
    pub fn new(framed: Framed<A::Io, A::Codec>) -> ActorFramedCell<A> {
        ActorFramedCell {
            flags: FramedFlags::SINK_FLUSHED,
            framed: framed,
            sink_items: VecDeque::new(),
            drain: None,
            error: None,
        }
    }

    pub fn replace(&mut self, framed: Framed<A::Io, A::Codec>) {
        self.framed = framed;
        self.flags = FramedFlags::SINK_FLUSHED;
        self.sink_items.clear();
        self.drain.take();
    }

    pub fn close(&mut self) {
        self.flags.insert(FramedFlags::CLOSING);
    }

    pub fn closed(&mut self) -> bool {
        self.flags.contains(FramedFlags::STREAM_CLOSED | FramedFlags::SINK_CLOSED)
    }

    pub fn send(&mut self, msg: <<A as FramedActor>::Codec as Encoder>::Item) {
        // try to write to sink immediately
        if self.sink_items.is_empty() && self.error.is_none() {
            self.flags.remove(FramedFlags::SINK_FLUSHED);
            match self.framed.start_send(msg) {
                Ok(AsyncSink::NotReady(msg)) =>
                    self.sink_items.push_front(msg),
                Ok(AsyncSink::Ready) => (),
                Err(err) => self.error = Some(err),
            }
        } else {
            self.sink_items.push_back(msg);
        }
    }

    pub fn drain(&mut self) -> UnsyncReceiver<()> {
        if let Some(tx) = self.drain.take() {
            let _ = tx.send(());
            error!("drain method should be called once");
        }
        let (tx, rx) = channel();
        self.drain = Some(tx);
        rx
    }

    pub fn into_framed(self) -> Framed<A::Io, A::Codec> {
        self.framed
    }
}

impl<A> ContextCell<A> for ActorFramedCell<A>
    where A: Actor + FramedActor, A::Context: AsyncContext<A>,
{
    fn alive(&self) -> bool {
        !self.flags.contains(FramedFlags::SINK_CLOSED) &&
            !self.flags.contains(FramedFlags::STREAM_CLOSED)
    }

    fn poll(&mut self, act: &mut A, ctx: &mut A::Context, stop: bool) -> ContextCellResult {
        if let Some(err) = self.error.take() {
            self.flags |= FramedFlags::SINK_CLOSED | FramedFlags::STREAM_CLOSED;
            <A as FramedActor>::closed(act, Some(err), ctx);
        }

        loop {
            let mut not_ready = true;

            // check framed stream
            if self.drain.is_none() && !self.flags.contains(FramedFlags::CLOSING) &&
                !self.flags.contains(FramedFlags::STREAM_CLOSED)
            {
                match self.framed.poll() {
                    Ok(Async::Ready(Some(msg))) => {
                        not_ready = false;
                        <A as FramedActor>::handle(act, Ok(msg), ctx);
                    }
                    Ok(Async::Ready(None)) => {
                        self.flags |= FramedFlags::SINK_CLOSED | FramedFlags::STREAM_CLOSED;
                        <A as FramedActor>::closed(act, None, ctx);
                    }
                    Ok(Async::NotReady) => (),
                    Err(err) => {
                        self.flags |= FramedFlags::SINK_CLOSED | FramedFlags::STREAM_CLOSED;
                        <A as FramedActor>::handle(act, Err(err), ctx);
                    }
                }
            }

            if !self.flags.contains(FramedFlags::SINK_CLOSED) {
                // send sink items
                loop {
                    if let Some(msg) = self.sink_items.pop_front() {
                        self.flags.remove(FramedFlags::SINK_FLUSHED);
                        match self.framed.start_send(msg) {
                            Ok(AsyncSink::NotReady(msg)) => {
                                self.sink_items.push_front(msg);
                                if self.drain.is_some() {
                                    return ContextCellResult::NotReady
                                }
                                break
                            }
                            Ok(AsyncSink::Ready) => {
                                continue
                            }
                            Err(err) => {
                                self.flags |=
                                    FramedFlags::SINK_CLOSED | FramedFlags::STREAM_CLOSED;
                                <A as FramedActor>::closed(act, Some(err), ctx);
                                break
                            }
                        }
                    }
                    break
                }

                // flush sink
                if !self.flags.contains(FramedFlags::SINK_FLUSHED) {
                    match self.framed.poll_complete() {
                        Ok(Async::Ready(_)) => {
                            not_ready = false;
                            self.flags |= FramedFlags::SINK_FLUSHED;
                        }
                        Ok(Async::NotReady) =>
                            if self.drain.is_some() {
                                return ContextCellResult::NotReady;
                            },
                        Err(err) => {
                            self.flags |= FramedFlags::SINK_CLOSED | FramedFlags::SINK_FLUSHED;
                            <A as FramedActor>::closed(act, Some(err), ctx);
                        }
                    }
                }

                if self.flags.contains(FramedFlags::CLOSING) &&
                    self.flags.contains(FramedFlags::SINK_FLUSHED) &&
                    self.sink_items.is_empty() {
                        self.flags |= FramedFlags::SINK_CLOSED;
                    }
            }
            if let Some(tx) = self.drain.take() {
                let _ = tx.send(());
            }

            // are we done
            if not_ready {
                if !self.alive() {
                    return ContextCellResult::Stop
                } else if stop {
                    if let Ok(Async::NotReady) = self.framed.get_mut().shutdown() {
                        return ContextCellResult::NotReady
                    }
                }
                return ContextCellResult::Ready
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cmp, io};
    use bytes::{Bytes, BytesMut};
    use tokio_io::AsyncWrite;
    use tokio_io::codec::{Encoder, Decoder};

    struct Buffer {
        buf: Bytes,
        eof: bool,
        write: BytesMut,
        err: Option<io::Error>,
        write_err: Option<io::Error>,
        write_block: bool,
    }

    impl Buffer {
        fn new(data: &'static str) -> Buffer {
            Buffer {
                buf: Bytes::from(data),
                eof: false,
                write: BytesMut::new(),
                err: None,
                write_err: None,
                write_block: false,
            }
        }
        fn feed_data(&mut self, data: &'static str) {
            let mut b = BytesMut::from(self.buf.as_ref());
            b.extend(data.as_bytes());
            self.buf = b.take().freeze();
        }
    }

    impl AsyncRead for Buffer {}
    impl io::Read for Buffer {
        fn read(&mut self, dst: &mut [u8]) -> Result<usize, io::Error> {
            if self.buf.is_empty() {
                if self.err.is_some() {
                    Err(self.err.take().unwrap())
                } else if self.eof {
                    Ok(0)
                } else {
                    Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
                }
            } else {
                let size = cmp::min(self.buf.len(), dst.len());
                let b = self.buf.split_to(size);
                dst[..size].copy_from_slice(&b);
                Ok(size)
            }
        }
    }

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.write_block {
                Err(io::Error::new(io::ErrorKind::WouldBlock, ""))
            } else if let Some(err) = self.write_err.take() {
                Err(err)
            } else {
                self.write.extend(buf);
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsyncWrite for Buffer {
        fn shutdown(&mut self) -> Poll<(), io::Error> {
            Ok(Async::Ready(()))
        }
    }

    struct Item(Bytes);
    impl ResponseType for Item {
        type Item = ();
        type Error = ();
    }

    struct TestCodec;

    impl Decoder for TestCodec {
        type Item = Item;
        type Error = io::Error;

        fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            if src.len() >= 2 {
                Ok(Some(Item(src.split_to(2).freeze())))
            } else {
                Ok(None)
            }
        }
    }

    impl Encoder for TestCodec {
        type Item = Bytes;
        type Error = io::Error;

        fn encode(&mut self, msg: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
            dst.extend(msg);
            Ok(())
        }
    }

    struct TestActor {
        msgs: Vec<Bytes>,
        closed: bool,
        error: Option<io::Error>,
    }

    impl TestActor {
        fn new() -> TestActor {
            TestActor{msgs: Vec::new(), closed: false, error: None}
        }
    }

    impl Actor for TestActor {
        type Context = FramedContext<Self>;
    }

    impl FramedActor for TestActor {
        type Io = Buffer;
        type Codec = TestCodec;

        fn handle(&mut self, msg: Result<Item, io::Error>, _: &mut Self::Context) {
            if let Ok(item) = msg {
                self.msgs.push(item.0);
            }
        }

        fn closed(&mut self, err: Option<io::Error>, _: &mut Self::Context) {
            self.error = err;
            self.closed = true;
        }
    }

    #[test]
    fn test_basic() {
        let mut ctx = FramedContext::new(
            Some(TestActor::new()), Buffer::new(""), TestCodec);

        let _ = ctx.poll();
        ctx.inner.cell().as_mut().unwrap().framed.get_mut().feed_data("data");

        // messages recevied
        let _ = ctx.poll();
        assert_eq!(ctx.inner.actor().msgs[0], b"da"[..]);
        assert_eq!(ctx.inner.actor().msgs[1], b"ta"[..]);

        // block sink
        ctx.inner.cell().as_mut().unwrap().framed.get_mut().write_block = true;
        ctx.send(Bytes::from_static(b"11")).ok().unwrap();
        ctx.send(Bytes::from_static(b"22")).ok().unwrap();

        // drain
        let _ = ctx.drain();

        // new data in framed, actor is paused
        ctx.inner.cell().as_mut().unwrap().framed.get_mut().feed_data("bb");
        let _ = ctx.poll();
        assert_eq!(ctx.inner.actor().msgs.len(), 2);

        // sink unblocked
        ctx.inner.cell().as_mut().unwrap().framed.get_mut().write_block = false;
        let _ = ctx.poll();
        assert_eq!(ctx.inner.actor().msgs.len(), 3);
        assert_eq!(ctx.inner.actor().msgs[2], b"bb"[..]);

        // sink data
        assert_eq!(ctx.inner.cell().as_mut().unwrap().framed.get_mut().write, b"1122"[..]);
    }

    #[test]
    fn test_multiple_message() {
        let mut ctx = FramedContext::new(
            Some(TestActor::new()), Buffer::new(""), TestCodec);

        let _ = ctx.poll();
        ctx.inner.cell().as_mut().unwrap().framed.get_mut().feed_data("11223344");

        // messages recevied
        let _ = ctx.poll();
        assert_eq!(ctx.inner.actor().msgs,
                   vec![Bytes::from_static(b"11"), Bytes::from_static(b"22"),
                        Bytes::from_static(b"33"), Bytes::from_static(b"44")]);
    }

    #[test]
    fn test_error_during_poll() {
        let mut ctx = FramedContext::new(
            Some(TestActor::new()), Buffer::new(""), TestCodec);

        let _ = ctx.poll();
        ctx.inner.cell().as_mut().unwrap().framed.get_mut().write_err =
            Some(io::Error::new(io::ErrorKind::Other, "error"));

        ctx.inner.cell().as_mut().unwrap().sink_items.push_back(Bytes::from_static(b"11"));
        let _ = ctx.poll();
        assert!(ctx.inner.actor().error.is_some());
        assert!(ctx.inner.actor().closed);
    }

    #[test]
    fn test_close() {
        let mut buf = Buffer::new("");
        buf.eof = true;
        let mut ctx = FramedContext::new(Some(TestActor::new()), buf, TestCodec);

        let _ = ctx.poll();
        assert!(ctx.inner.actor().error.is_none());
        assert!(ctx.inner.actor().closed);
    }
}
