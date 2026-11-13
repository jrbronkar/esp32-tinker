//! Request and response handles driven by a single connection task on a local executor.

use alloc::{rc::Rc, vec::Vec};
use core::{
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    mem,
    pin::pin,
    task::{Poll, Waker},
};
use embedded_io_async::{Read, Write};
use zerodds_http2::{ErrorCode, StreamId};

use crate::{BodyFrame, Error, Event, HeaderField, MAX_STREAMS, RequestHead, Server};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamError {
    Reset(u32),
    Disconnected,
    HeaderLimit,
}

#[derive(Default)]
struct Signal(RefCell<Option<Waker>>);

impl Signal {
    fn register(&self, waker: &Waker) {
        let mut slot = self.0.borrow_mut();
        if let Some(current) = slot.as_mut() {
            current.clone_from(waker);
        } else {
            *slot = Some(waker.clone());
        }
    }

    fn wake(&self) {
        let waker = self.0.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

struct Shared {
    pending: RefCell<Vec<Request>>,
    incoming: Signal,
    driver: Rc<Signal>,
    closed: Cell<bool>,
    abandoned: Cell<bool>,
}

#[derive(Default)]
struct ReceiveState {
    bytes: Vec<u8>,
    trailers: Option<Vec<HeaderField>>,
    ended: bool,
    held: bool,
    needs_credit: bool,
    dropped: bool,
    error: Option<StreamError>,
}

enum WriteCommand {
    Head(Vec<HeaderField>),
    HeadersOnly(Vec<HeaderField>),
    Data(Vec<u8>, usize),
    Finish(Vec<HeaderField>),
}

#[derive(Default)]
struct SendState {
    command: Option<WriteCommand>,
    writing: bool,
    finished: bool,
    canceled: bool,
}

struct Receive {
    state: RefCell<ReceiveState>,
    reader: Signal,
    send: RefCell<SendState>,
    writer: Signal,
    cancellation: Signal,
    driver: Rc<Signal>,
}

pub struct Request {
    pub head: RequestHead,
    pub body: RecvStream,
    pub respond: SendResponse,
}

pub struct SendResponse(Option<Rc<Receive>>);
pub struct SendStream(Option<Rc<Receive>>);

pub struct Incoming(Rc<Shared>);

pub struct RecvStream(Rc<Receive>);

/// Bytes whose receive credit remains outstanding until the chunk is dropped.
pub struct Chunk {
    bytes: Vec<u8>,
    receive: Rc<Receive>,
}

pub struct Connection<Io> {
    transport: Server<Io>,
    shared: Rc<Shared>,
    streams: Vec<(StreamId, Rc<Receive>)>,
}

/// Start a connection and obtain its request queue.
///
/// Run the driver and handlers on the same executor thread. The socket's
/// `Read::read` must be cancellation-safe because handler work can interrupt
/// a pending read. The driver keeps completed frame and header fragments.
pub async fn handshake<Io: Read + Write>(
    io: Io,
) -> Result<(Connection<Io>, Incoming), Error<Io::Error>> {
    let transport = Server::handshake(io).await?;
    let shared = Rc::new(Shared {
        pending: RefCell::new(Vec::with_capacity(MAX_STREAMS)),
        incoming: Signal::default(),
        driver: Rc::new(Signal::default()),
        closed: Cell::new(false),
        abandoned: Cell::new(false),
    });
    Ok((
        Connection {
            transport,
            shared: shared.clone(),
            streams: Vec::with_capacity(MAX_STREAMS),
        },
        Incoming(shared),
    ))
}

impl Incoming {
    pub async fn accept(&mut self) -> Option<Request> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    pub(crate) fn poll_accept(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> Poll<Option<Request>> {
        if self.0.closed.get() {
            return Poll::Ready(None);
        }
        let mut pending = self.0.pending.borrow_mut();
        if pending.is_empty() {
            self.0.incoming.register(cx.waker());
            Poll::Pending
        } else {
            Poll::Ready(Some(pending.remove(0)))
        }
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.0.abandoned.set(true);
        self.0.pending.borrow_mut().clear();
        self.0.driver.wake();
    }
}

impl SendResponse {
    /// Wait for the peer to reset this stream or disconnect before responding.
    pub async fn closed(&self) {
        let Some(stream) = &self.0 else { return };
        wait_for_close(stream).await
    }

    /// Send a headers-only response after the request half-closes.
    pub async fn finish_headers(mut self, headers: Vec<HeaderField>) -> Result<(), StreamError> {
        let stream = self.0.as_ref().ok_or(StreamError::Disconnected)?;
        check_header_size(&headers)?;
        if let Some(error) = stream.state.borrow().error {
            return Err(error);
        }
        stream.send.borrow_mut().command = Some(WriteCommand::HeadersOnly(headers));
        stream.driver.wake();
        wait_for_finish(stream).await?;
        self.0.take();
        Ok(())
    }

    /// Queue the response head and obtain a handle for DATA and trailers.
    pub fn send_response(mut self, headers: Vec<HeaderField>) -> Result<SendStream, StreamError> {
        let stream = self.0.as_ref().ok_or(StreamError::Disconnected)?;
        if let Some(error) = stream.state.borrow().error {
            return Err(error);
        }
        check_header_size(&headers)?;
        stream.send.borrow_mut().command = Some(WriteCommand::Head(headers));
        stream.driver.wake();
        Ok(SendStream(self.0.take()))
    }
}

fn check_header_size(headers: &[HeaderField]) -> Result<(), StreamError> {
    let size = headers
        .iter()
        .fold(0usize, |size, field| size.saturating_add(field.size()));
    if size > crate::headers::MAX_LIST {
        Err(StreamError::HeaderLimit)
    } else {
        Ok(())
    }
}

impl SendStream {
    /// Wait for peer reset or disconnection during a streaming response.
    pub async fn closed(&self) {
        if let Some(stream) = &self.0 {
            wait_for_close(stream).await;
        }
    }

    /// Observe cancellation independently of an in-flight send or receive.
    pub fn cancellation(&self) -> impl Future<Output = ()> + use<> {
        let stream = self.0.clone();
        async move {
            if let Some(stream) = stream {
                wait_for_close(&stream).await;
            }
        }
    }

    /// Wait until all previously queued response bytes have been flushed.
    pub async fn flush(&mut self) -> Result<(), StreamError> {
        let stream = self.0.as_ref().ok_or(StreamError::Disconnected)?;
        Self::wait_for_slot(stream).await
    }

    /// Wait for the outbound slot and enqueue up to one frame of DATA.
    pub async fn send_data(&mut self, bytes: &[u8]) -> Result<usize, StreamError> {
        let stream = self.0.as_ref().ok_or(StreamError::Disconnected)?;
        let count = bytes
            .len()
            .min(zerodds_http2::frame::DEFAULT_MAX_FRAME_SIZE as usize);
        if count == 0 {
            return Ok(0);
        }
        Self::wait_for_slot(stream).await?;
        stream.send.borrow_mut().command = Some(WriteCommand::Data(bytes[..count].to_vec(), 0));
        stream.driver.wake();
        Ok(count)
    }

    /// Write final trailers after the request half-closes or its receive handle drops.
    ///
    /// Success means the final frame has been flushed to the transport.
    pub async fn finish(mut self, trailers: Vec<HeaderField>) -> Result<(), StreamError> {
        let stream = self.0.as_ref().ok_or(StreamError::Disconnected)?;
        check_header_size(&trailers)?;
        Self::wait_for_slot(stream).await?;
        stream.send.borrow_mut().command = Some(WriteCommand::Finish(trailers));
        stream.driver.wake();
        wait_for_finish(stream).await?;
        self.0.take();
        Ok(())
    }

    async fn wait_for_slot(stream: &Receive) -> Result<(), StreamError> {
        poll_fn(|cx| {
            let send = stream.send.borrow();
            if let Some(error) = stream.state.borrow().error {
                Poll::Ready(Err(error))
            } else if send.command.is_none() && !send.writing {
                Poll::Ready(Ok(()))
            } else {
                stream.writer.register(cx.waker());
                Poll::Pending
            }
        })
        .await
    }
}

async fn wait_for_close(stream: &Receive) {
    poll_fn(|cx| {
        if stream.state.borrow().error.is_some() {
            Poll::Ready(())
        } else {
            stream.cancellation.register(cx.waker());
            Poll::Pending
        }
    })
    .await
}

async fn wait_for_finish(stream: &Receive) -> Result<(), StreamError> {
    poll_fn(|cx| {
        if stream.send.borrow().finished {
            Poll::Ready(Ok(()))
        } else if let Some(error) = stream.state.borrow().error {
            Poll::Ready(Err(error))
        } else {
            stream.writer.register(cx.waker());
            Poll::Pending
        }
    })
    .await
}

fn cancel(stream: Option<&Rc<Receive>>) {
    if let Some(stream) = stream {
        stream.send.borrow_mut().canceled = true;
        stream.driver.wake();
    }
}

impl Drop for SendResponse {
    fn drop(&mut self) {
        cancel(self.0.as_ref());
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        cancel(self.0.as_ref());
    }
}

impl RecvStream {
    /// Receive a chunk. A chunk may combine several DATA frames.
    pub async fn data(&mut self) -> Result<Option<Chunk>, StreamError> {
        poll_fn(|cx| {
            let mut state = self.0.state.borrow_mut();
            if let Some(error) = state.error {
                return Poll::Ready(Err(error));
            }
            if !state.held && !state.bytes.is_empty() {
                state.held = true;
                return Poll::Ready(Ok(Some(Chunk {
                    bytes: mem::take(&mut state.bytes),
                    receive: self.0.clone(),
                })));
            }
            if state.ended && state.bytes.is_empty() {
                return Poll::Ready(Ok(None));
            }
            self.0.reader.register(cx.waker());
            Poll::Pending
        })
        .await
    }

    /// Take trailers after draining the body.
    pub fn trailers(&mut self) -> Option<Vec<HeaderField>> {
        let mut state = self.0.state.borrow_mut();
        if state.bytes.is_empty() {
            state.trailers.take()
        } else {
            None
        }
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        let mut state = self.0.state.borrow_mut();
        state.dropped = true;
        state.bytes.clear();
        drop(state);
        self.0.driver.wake();
    }
}

impl Chunk {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Chunk {
    fn drop(&mut self) {
        self.receive.state.borrow_mut().held = false;
        self.receive.driver.wake();
        self.receive.reader.wake();
    }
}

impl<Io> Drop for Connection<Io> {
    fn drop(&mut self) {
        self.shared.closed.set(true);
        for (_, receive) in &self.streams {
            fail(receive, StreamError::Disconnected);
        }
        self.shared.pending.borrow_mut().clear();
        self.shared.incoming.wake();
    }
}

fn fail(receive: &Receive, error: StreamError) {
    {
        let mut state = receive.state.borrow_mut();
        state.error = Some(error);
        state.bytes = Vec::new();
        state.trailers = None;
    }
    receive.send.borrow_mut().command = None;
    receive.reader.wake();
    receive.writer.wake();
    receive.cancellation.wake();
}

impl<Io: Read + Write> Connection<Io> {
    /// Advance the socket until the peer disconnects or the acceptor drops.
    pub async fn run(mut self) -> Result<(), Error<Io::Error>> {
        loop {
            if self.shared.abandoned.get() {
                return Ok(());
            }
            self.apply_receive_work().await?;
            self.apply_send_work().await?;
            let ready = self
                .streams
                .iter()
                .any(|(id, stream)| self.send_ready(*id, stream));
            if ready {
                continue;
            }
            let frame = {
                let send_capacity: [_; MAX_STREAMS] = core::array::from_fn(|index| {
                    self.streams.get(index).is_some_and(|(id, _)| {
                        self.transport.send_capacity(*id).is_ok_and(|n| n > 0)
                    })
                });
                let mut read = pin!(self.transport.read_inbound());
                poll_fn(|cx| {
                    self.shared.driver.register(cx.waker());
                    if self.shared.abandoned.get()
                        || self
                            .streams
                            .iter()
                            .enumerate()
                            .any(|(index, (_, receive))| {
                                let state = receive.state.borrow();
                                let send = receive.send.borrow();
                                send.canceled
                                    || (state.needs_credit && state.bytes.is_empty() && !state.held)
                                    || command_ready(
                                        &send,
                                        state.ended,
                                        state.dropped,
                                        send_capacity[index],
                                    )
                            })
                    {
                        return Poll::Ready(None);
                    }
                    read.as_mut().poll(cx).map(Some)
                })
                .await
            };
            let Some(frame) = frame else { continue };
            match self.transport.dispatch(frame?).await? {
                Event::Request(head) => {
                    let receive = Rc::new(Receive {
                        state: RefCell::new(ReceiveState {
                            ended: head.end_stream,
                            ..ReceiveState::default()
                        }),
                        reader: Signal::default(),
                        send: RefCell::new(SendState::default()),
                        writer: Signal::default(),
                        cancellation: Signal::default(),
                        driver: self.shared.driver.clone(),
                    });
                    self.streams.push((head.stream_id, receive.clone()));
                    self.shared.pending.borrow_mut().push(Request {
                        head,
                        body: RecvStream(receive.clone()),
                        respond: SendResponse(Some(receive)),
                    });
                    self.shared.incoming.wake();
                }
                Event::Body {
                    stream_id,
                    frame,
                    end_stream,
                } => {
                    let (_, receive) = self
                        .streams
                        .iter()
                        .find(|(id, _)| *id == stream_id)
                        .ok_or(Error::Protocol)?;
                    {
                        let mut state = receive.state.borrow_mut();
                        // The connection receive window bounds queued and held
                        // bytes together. Credit is returned after both drain.
                        match frame {
                            BodyFrame::Data(bytes) => {
                                if !state.dropped {
                                    state.bytes.extend_from_slice(bytes);
                                }
                                state.needs_credit = true;
                            }
                            BodyFrame::Trailers(trailers) => state.trailers = Some(trailers),
                        }
                        state.ended = end_stream;
                    }
                    receive.reader.wake();
                }
                Event::Reset { stream_id, code } => {
                    if let Some(index) = self.streams.iter().position(|(id, _)| *id == stream_id) {
                        let (_, receive) = self.streams.swap_remove(index);
                        fail(&receive, StreamError::Reset(code));
                        self.shared
                            .pending
                            .borrow_mut()
                            .retain(|request| request.head.stream_id != stream_id);
                    }
                }
                Event::Control => {}
            }
            // Let local handlers run even when the socket already has more frames.
            let mut yielded = false;
            poll_fn(|cx| {
                if core::mem::replace(&mut yielded, true) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        }
    }

    async fn apply_receive_work(&mut self) -> Result<(), Error<Io::Error>> {
        let mut index = 0;
        while let Some((id, receive)) = self.streams.get(index) {
            let id = *id;
            if receive.send.borrow().canceled {
                self.transport.reset_stream(id, ErrorCode::Cancel).await?;
                let (_, receive) = self.streams.swap_remove(index);
                fail(&receive, StreamError::Reset(ErrorCode::Cancel as u32));
                continue;
            }
            let release = {
                let mut state = receive.state.borrow_mut();
                !state.held && state.bytes.is_empty() && mem::take(&mut state.needs_credit)
            };
            if release {
                self.transport.release_capacity(id).await?;
            }
            index += 1;
        }
        Ok(())
    }

    fn send_ready(&self, id: StreamId, stream: &Receive) -> bool {
        let receive = stream.state.borrow();
        command_ready(
            &stream.send.borrow(),
            receive.ended,
            receive.dropped,
            self.transport.send_capacity(id).is_ok_and(|n| n > 0),
        )
    }

    async fn apply_send_work(&mut self) -> Result<(), Error<Io::Error>> {
        let mut index = 0;
        while let Some((id, stream)) = self.streams.get(index) {
            let id = *id;
            if !self.send_ready(id, stream) {
                index += 1;
                continue;
            }
            let command = {
                let mut send = stream.send.borrow_mut();
                send.writing = true;
                send.command.take().ok_or(Error::Protocol)?
            };
            let finishing = matches!(
                command,
                WriteCommand::Finish(_) | WriteCommand::HeadersOnly(_)
            );
            let result = match command {
                WriteCommand::Head(headers) => self.transport.start_response(id, &headers).await,
                WriteCommand::Data(bytes, offset) => {
                    let sent = self.transport.send_data(id, &bytes[offset..]).await?;
                    if offset + sent < bytes.len() {
                        stream.send.borrow_mut().writing = false;
                        stream.send.borrow_mut().command =
                            Some(WriteCommand::Data(bytes, offset + sent));
                        index += 1;
                        continue;
                    }
                    Ok(())
                }
                WriteCommand::Finish(trailers) => {
                    self.transport.finish_stream_response(id, &trailers).await
                }
                WriteCommand::HeadersOnly(headers) => {
                    self.transport
                        .respond(
                            id,
                            crate::Response {
                                headers: &headers,
                                body: &[],
                                trailers: &[],
                            },
                        )
                        .await
                }
            };
            if let Err(error) = result {
                if !matches!(error, Error::HeaderBlock | Error::Protocol) {
                    return Err(error);
                }
                self.transport
                    .reset_stream(id, ErrorCode::InternalError)
                    .await?;
                let (_, stream) = self.streams.swap_remove(index);
                fail(&stream, StreamError::Reset(ErrorCode::InternalError as u32));
                continue;
            }
            let mut send = stream.send.borrow_mut();
            send.writing = false;
            send.finished = finishing;
            drop(send);
            stream.writer.wake();
            if finishing {
                self.streams.swap_remove(index);
            } else {
                index += 1;
            }
        }
        Ok(())
    }
}

fn command_ready(send: &SendState, ended: bool, dropped: bool, capacity: bool) -> bool {
    match &send.command {
        Some(WriteCommand::Head(_)) => true,
        Some(WriteCommand::Data(..)) => capacity,
        Some(WriteCommand::Finish(_)) => ended || dropped,
        Some(WriteCommand::HeadersOnly(_)) => ended,
        None => false,
    }
}
