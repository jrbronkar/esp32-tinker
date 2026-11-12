use core::convert::Infallible;
use core::task::{Context, Poll, Waker};
use std::{boxed::Box, cell::RefCell, collections::VecDeque, mem, rc::Rc, vec::Vec};

use crate::Transport;
use crate::protocol::{self, Packet};
use crate::sync;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tests) struct MockError(pub(in crate::tests) &'static str);

type Filter = Box<dyn FnMut(usize, &[u8]) -> Vec<u8>>;

#[derive(Default)]
struct Pipe {
    queue: VecDeque<u8>,
    partial: Vec<u8>,
    log: Vec<Vec<u8>>,
    filter: Option<Filter>,
    closed: bool,
    reader: Option<Waker>,
}

impl Pipe {
    fn push(&mut self, byte: u8) {
        if byte != 0 {
            self.partial.push(byte);
            return;
        }
        if self.partial.is_empty() {
            self.queue.push_back(0);
            return;
        }
        let mut wire = mem::take(&mut self.partial);
        wire.push(0);
        let index = self.log.len();
        self.log.push(wire.clone());
        let delivered = match self.filter.as_mut() {
            Some(filter) => filter(index, &wire),
            None => wire,
        };
        self.queue.extend(delivered);
    }

    fn wake_reader(&mut self) {
        if let Some(waker) = self.reader.take() {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct Control {
    read_chunk: usize,
    write_chunk: usize,
    reads_blocked: bool,
    writes_blocked: bool,
    write_budget: Option<usize>,
    flush_blocked: bool,
    read_error: Option<MockError>,
    write_error: Option<MockError>,
    flush_error: Option<MockError>,
    write_zero: bool,
    gate_waker: Option<Waker>,
}

#[derive(Clone)]
pub(in crate::tests) struct Endpoint {
    incoming: Rc<RefCell<Pipe>>,
    outgoing: Rc<RefCell<Pipe>>,
    control: Rc<RefCell<Control>>,
}

pub(in crate::tests) fn link() -> (Endpoint, Endpoint) {
    let a_to_b = Rc::new(RefCell::new(Pipe::default()));
    let b_to_a = Rc::new(RefCell::new(Pipe::default()));
    (
        Endpoint::new(b_to_a.clone(), a_to_b.clone()),
        Endpoint::new(a_to_b, b_to_a),
    )
}

impl Endpoint {
    fn new(incoming: Rc<RefCell<Pipe>>, outgoing: Rc<RefCell<Pipe>>) -> Self {
        Self {
            incoming,
            outgoing,
            control: Rc::new(RefCell::new(Control {
                read_chunk: usize::MAX,
                write_chunk: usize::MAX,
                ..Control::default()
            })),
        }
    }

    pub(in crate::tests) fn set_read_chunk(&self, chunk: usize) {
        self.control.borrow_mut().read_chunk = chunk;
    }

    pub(in crate::tests) fn set_write_chunk(&self, chunk: usize) {
        self.control.borrow_mut().write_chunk = chunk;
    }

    pub(in crate::tests) fn block_writes(&self, blocked: bool) {
        let mut control = self.control.borrow_mut();
        control.writes_blocked = blocked;
        Self::release(&mut control, blocked);
    }

    pub(in crate::tests) fn limit_writes(&self, budget: Option<usize>) {
        let mut control = self.control.borrow_mut();
        control.write_budget = budget;
        Self::release(&mut control, budget == Some(0));
    }

    pub(in crate::tests) fn block_flush(&self, blocked: bool) {
        let mut control = self.control.borrow_mut();
        control.flush_blocked = blocked;
        Self::release(&mut control, blocked);
    }

    pub(in crate::tests) fn block_reads(&self, blocked: bool) {
        let mut control = self.control.borrow_mut();
        control.reads_blocked = blocked;
        Self::release(&mut control, blocked);
    }

    fn release(control: &mut Control, blocked: bool) {
        if blocked {
            return;
        }
        if let Some(waker) = control.gate_waker.take() {
            waker.wake();
        }
    }

    pub(in crate::tests) fn fail_next_read(&self, error: MockError) {
        self.control.borrow_mut().read_error = Some(error);
    }

    pub(in crate::tests) fn fail_next_write(&self, error: MockError) {
        self.control.borrow_mut().write_error = Some(error);
    }

    pub(in crate::tests) fn fail_next_flush(&self, error: MockError) {
        self.control.borrow_mut().flush_error = Some(error);
    }

    pub(in crate::tests) fn accept_zero_bytes(&self, enabled: bool) {
        self.control.borrow_mut().write_zero = enabled;
    }

    pub(in crate::tests) fn filter_outgoing(
        &self,
        filter: impl FnMut(usize, &[u8]) -> Vec<u8> + 'static,
    ) {
        self.outgoing.borrow_mut().filter = Some(Box::new(filter));
    }

    pub(in crate::tests) fn inject_incoming(&self, bytes: &[u8]) {
        let mut pipe = self.incoming.borrow_mut();
        pipe.queue.extend(bytes);
        pipe.wake_reader();
    }

    pub(in crate::tests) fn close_incoming(&self) {
        let mut pipe = self.incoming.borrow_mut();
        pipe.closed = true;
        pipe.wake_reader();
    }

    pub(in crate::tests) fn written_frames(&self) -> Vec<Vec<u8>> {
        self.outgoing.borrow().log.clone()
    }
}

impl Transport for Endpoint {
    type Error = MockError;

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Self::Error>> {
        let mut control = self.control.borrow_mut();
        if let Some(error) = control.read_error.take() {
            return Poll::Ready(Err(error));
        }
        if control.reads_blocked {
            control.gate_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let mut pipe = self.incoming.borrow_mut();
        if pipe.queue.is_empty() {
            if pipe.closed {
                return Poll::Ready(Ok(0));
            }
            pipe.reader = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let len = buf.len().min(control.read_chunk).min(pipe.queue.len());
        for slot in &mut buf[..len] {
            *slot = pipe.queue.pop_front().unwrap();
        }
        Poll::Ready(Ok(len))
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Self::Error>> {
        let mut control = self.control.borrow_mut();
        if let Some(error) = control.write_error.take() {
            return Poll::Ready(Err(error));
        }
        if control.write_zero {
            return Poll::Ready(Ok(0));
        }
        if control.writes_blocked || control.write_budget == Some(0) {
            control.gate_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let len = buf.len().min(control.write_chunk);
        let len = control.write_budget.map_or(len, |budget| len.min(budget));
        if let Some(budget) = control.write_budget.as_mut() {
            *budget -= len;
        }
        let mut pipe = self.outgoing.borrow_mut();
        for &byte in &buf[..len] {
            pipe.push(byte);
        }
        pipe.wake_reader();
        Poll::Ready(Ok(len))
    }

    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut control = self.control.borrow_mut();
        if let Some(error) = control.flush_error.take() {
            return Poll::Ready(Err(error));
        }
        if control.flush_blocked {
            control.gate_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }
}

#[derive(Default)]
struct TrickleState {
    reads: usize,
    writes: usize,
    flushes: usize,
    written: Vec<u8>,
    incoming: VecDeque<u8>,
    reader: Option<Waker>,
}

/// A transport that counts calls and accepts one byte per write.
#[derive(Clone, Default)]
pub(in crate::tests) struct Trickle(Rc<RefCell<TrickleState>>);

impl Trickle {
    pub(in crate::tests) fn calls(&self) -> usize {
        let state = self.0.borrow();
        state.reads + state.writes + state.flushes
    }

    pub(in crate::tests) fn written(&self) -> Vec<u8> {
        self.0.borrow().written.clone()
    }

    pub(in crate::tests) fn inject(&self, bytes: &[u8]) {
        let mut state = self.0.borrow_mut();
        state.incoming.extend(bytes);
        if let Some(waker) = state.reader.take() {
            waker.wake();
        }
    }
}

impl Transport for Trickle {
    type Error = Infallible;

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Infallible>> {
        let mut state = self.0.borrow_mut();
        state.reads += 1;
        if state.incoming.is_empty() {
            state.reader = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let len = buf.len().min(state.incoming.len());
        for slot in &mut buf[..len] {
            *slot = state.incoming.pop_front().unwrap();
        }
        Poll::Ready(Ok(len))
    }

    fn poll_write(&mut self, _: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Infallible>> {
        let mut state = self.0.borrow_mut();
        state.writes += 1;
        state.written.push(buf[0]);
        Poll::Ready(Ok(1))
    }

    fn poll_flush(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.0.borrow_mut().flushes += 1;
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tests) enum Wire {
    Data {
        session: u64,
        seq: u32,
        payload: Vec<u8>,
    },
    Ack {
        session: u64,
        seq: u32,
    },
}

pub(in crate::tests) fn parse_wire(wire: &[u8]) -> Option<Wire> {
    let mut decoded = [0u8; 512];
    let frame = sync::decode_to_slice(wire, &mut decoded).ok()?;
    if frame.consumed != wire.len() {
        return None;
    }
    Some(match protocol::parse(&decoded[..frame.len])? {
        Packet::Ack { session, seq } => Wire::Ack { session, seq },
        Packet::Data {
            session,
            seq,
            payload,
        } => Wire::Data {
            session,
            seq,
            payload: payload.to_vec(),
        },
    })
}

pub(in crate::tests) fn is_data(wire: &[u8]) -> bool {
    matches!(parse_wire(wire), Some(Wire::Data { .. }))
}

pub(in crate::tests) fn is_ack(wire: &[u8]) -> bool {
    matches!(parse_wire(wire), Some(Wire::Ack { .. }))
}
