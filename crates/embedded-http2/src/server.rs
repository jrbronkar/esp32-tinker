//! Request handles driven by a single connection task on a local executor.

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

struct Receive {
    state: RefCell<ReceiveState>,
    reader: Signal,
    driver: Rc<Signal>,
}

pub struct Request {
    pub head: RequestHead,
    pub body: RecvStream,
}

pub struct Incoming(Rc<Shared>);

pub struct RecvStream(Rc<Receive>);

/// Bytes whose receive credit remains outstanding until the chunk is dropped.
pub struct Chunk<'a> {
    bytes: Vec<u8>,
    receive: &'a Receive,
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
        poll_fn(|cx| {
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
        })
        .await
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.0.abandoned.set(true);
        self.0.pending.borrow_mut().clear();
        self.0.driver.wake();
    }
}

impl RecvStream {
    /// Receive a chunk. A chunk may combine several DATA frames.
    pub async fn data(&mut self) -> Result<Option<Chunk<'_>>, StreamError> {
        poll_fn(|cx| {
            let mut state = self.0.state.borrow_mut();
            if let Some(error) = state.error {
                return Poll::Ready(Err(error));
            }
            if !state.bytes.is_empty() {
                state.held = true;
                return Poll::Ready(Ok(Some(Chunk {
                    bytes: mem::take(&mut state.bytes),
                    receive: &self.0,
                })));
            }
            if state.ended {
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
        self.0.state.borrow_mut().dropped = true;
        self.0.driver.wake();
    }
}

impl Chunk<'_> {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Chunk<'_> {
    fn drop(&mut self) {
        self.receive.state.borrow_mut().held = false;
        self.receive.driver.wake();
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
    receive.reader.wake();
}

impl<Io: Read + Write> Connection<Io> {
    /// Advance the socket until the peer disconnects or the acceptor drops.
    pub async fn run(mut self) -> Result<(), Error<Io::Error>> {
        loop {
            if self.shared.abandoned.get() {
                return Ok(());
            }
            self.apply_receive_work().await?;
            let frame = {
                let mut read = pin!(self.transport.read_inbound());
                poll_fn(|cx| {
                    self.shared.driver.register(cx.waker());
                    if self.shared.abandoned.get()
                        || self.streams.iter().any(|(_, receive)| {
                            let state = receive.state.borrow();
                            state.dropped
                                || (state.needs_credit && state.bytes.is_empty() && !state.held)
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
                        driver: self.shared.driver.clone(),
                    });
                    self.streams.push((head.stream_id, receive.clone()));
                    self.shared.pending.borrow_mut().push(Request {
                        head,
                        body: RecvStream(receive),
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
                                state.bytes.extend_from_slice(bytes);
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
        }
    }

    async fn apply_receive_work(&mut self) -> Result<(), Error<Io::Error>> {
        let mut index = 0;
        while let Some((id, receive)) = self.streams.get(index) {
            let id = *id;
            if receive.state.borrow().dropped {
                self.transport.reset_stream(id, ErrorCode::Cancel).await?;
                self.streams.swap_remove(index);
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
}
