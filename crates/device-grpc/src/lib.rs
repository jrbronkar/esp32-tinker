#![no_std]
extern crate alloc;
pub mod logs;
pub mod watchdog;
pub use protolink;

use alloc::vec::Vec;
use ota_protocol::{StatusResponse, UploadOperation};
use protolink::{CallContext, Handler, MethodKind, Route, ServerConfig, Status};

pub const PORT: u16 = ota_protocol::PORT;
pub const STATUS_PATH: &str = "/esp.experimental.Management/Status";

// Prototype resource limits; these do not establish a worst-case heap bound.
const MAX_CONCURRENT_STREAMS: u32 = 2;
const STREAM_WINDOW_BYTES: u32 = 8192;
// HTTP/2 cannot reduce the initial connection window below 65535.
const CONNECTION_WINDOW_BYTES: u32 = 65_535;
const MAX_HEADER_LIST_BYTES: u32 = 1024;
const MAX_PENDING_OUTPUT_BYTES: usize = 8192;

/// Apply the management service's bounded HTTP/2 and message limits.
pub fn config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.http2.max_concurrent_streams = MAX_CONCURRENT_STREAMS;
    config.http2.initial_window_size = STREAM_WINDOW_BYTES;
    config.http2.connection_window_size = CONNECTION_WINDOW_BYTES;
    config.http2.max_header_list_size = MAX_HEADER_LIST_BYTES;
    config.http2.max_pending_output_bytes = MAX_PENDING_OUTPUT_BYTES;
    config.max_message_size = ota_protocol::UPLOAD_FRAME;
    config
}

/// Unary gRPC status handler backed by a status provider.
pub struct Management<F>(pub F);
impl<F: FnMut() -> StatusResponse> Handler for Management<F> {
    fn route(&self, path: &str) -> Option<Route> {
        (path == STATUS_PATH).then_some(Route::new(MethodKind::Unary, 0))
    }
    fn call(&mut self, _: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        status_response(request, || (self.0)(), || 0)
    }
}

fn status_response(
    request: &[u8],
    status: impl FnOnce() -> StatusResponse,
    now: impl Fn() -> u64,
) -> Result<Vec<u8>, Status> {
    let started = now();
    let options: ota_protocol::StatusRequest = ota_protocol::decode(request)
        .map_err(|_| Status::invalid_argument("invalid status request"))?;
    let decoded = now();
    let mut current = status();
    if !options.include_boot {
        current.clear_boot();
    }
    if options.include_timings {
        current.set_timings(ota_protocol::DeviceTimings {
            decode_us: decoded.saturating_sub(started),
            prepare_us: now().saturating_sub(decoded),
        });
    } else {
        current.clear_timings();
    }
    encode(&current)
}

use core::{
    cell::{RefCell, RefMut},
    task::{Context, Poll},
};
use ota_protocol::{ReadLogsRequest, ReadLogsResponse, UploadAck, UploadPhase};
use protolink::{CallId, Next};
pub const READ_LOGS_PATH: &str = "/esp.experimental.Management/ReadLogs";
pub const LOGS_PATH: &str = "/esp.experimental.Management/Logs";
pub const UPLOAD_PATH: &str = "/esp.experimental.Management/Upload";

/// Typed lifecycle notifications; log text is presentation, never control flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadEvent {
    Receiving,
    ChunkWritten { request_id: u32, next_offset: u32 },
    Verified,
    Released,
    ActivationPending,
    Committed,
}
impl core::fmt::Display for UploadEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Receiving => f.write_str("gRPC upload receiving"),
            Self::ChunkWritten {
                request_id,
                next_offset,
            } => write!(f, "gRPC chunk id={request_id} next={next_offset}"),
            Self::Verified => f.write_str("gRPC image verified"),
            Self::Released => f.write_str("gRPC upload released"),
            Self::ActivationPending => f.write_str("gRPC activation pending"),
            Self::Committed => f.write_str("gRPC upload committed"),
        }
    }
}

/// External log producers must wake the registered task when records arrive.
pub trait Environment {
    fn status(&self) -> StatusResponse;
    fn logs(&self, cursor: &ReadLogsRequest, cx: &mut Context<'_>) -> ReadLogsResponse;
    fn read_logs(&self, cursor: &ReadLogsRequest) -> ReadLogsResponse;
    fn now_us(&self) -> u64 {
        0
    }
    fn event(&self, event: UploadEvent);
    fn activation_pending(&self) -> bool {
        false
    }
}
struct Subscription {
    id: CallId,
    cursor: ReadLogsRequest,
    first: bool,
}
struct Upload<'a, T> {
    id: CallId,
    last_id: u32,
    target: RefMut<'a, T>,
    update: ota_core::Update,
    pending: Option<Vec<u8>>,
    ended: bool,
    verified: bool,
}
/// One connection, bounded subscription state and a single exclusive flash lease.
/// Competing streams fail admission before touching flash. Dropping the driver
/// releases the lease; activation remains latched until reboot.
pub struct StreamingManagement<'a, T: ota_core::Target, E: Environment> {
    target: &'a RefCell<T>,
    target_offset: u32,
    activation_pending: bool,
    env: E,
    upload: Option<Upload<'a, T>>,
    subscriptions: [Option<Subscription>; 2],
}
impl<'a, T: ota_core::Target, E: Environment> StreamingManagement<'a, T, E> {
    pub fn new(target: &'a RefCell<T>, target_offset: u32, env: E) -> Self {
        Self {
            target,
            target_offset,
            activation_pending: false,
            env,
            upload: None,
            subscriptions: [None, None],
        }
    }
    fn release(&mut self) {
        if self.upload.take().is_some() && !self.activation_pending {
            self.env.event(UploadEvent::Released);
        }
    }
    fn message(&mut self, ctx: &CallContext<'_>, bytes: &[u8]) -> Result<(), Status> {
        if ctx.route.method == 1 {
            let cursor = ota_protocol::decode(bytes)
                .map_err(|_| Status::invalid_argument("invalid log cursor"))?;
            let slot = self
                .subscriptions
                .iter_mut()
                .find(|s| s.is_none())
                .ok_or_else(|| Status::resource_exhausted("too many subscriptions"))?;
            *slot = Some(Subscription {
                id: ctx.id,
                cursor,
                first: true,
            });
            return Ok(());
        }
        let request = ota_protocol::decode_upload(bytes)
            .map_err(|_| Status::invalid_argument("invalid upload request"))?;
        if self.activation_pending || self.env.activation_pending() {
            return Err(Status::failed_precondition(
                "activation pending; reconnect after reboot",
            ));
        }
        let op = request
            .payload
            .ok_or_else(|| Status::invalid_argument("missing upload operation"))?;
        let (phase, offset) = if let Some(upload) = self.upload.as_mut() {
            if upload.id != ctx.id {
                return Err(Status::resource_exhausted("flash owned by another upload"));
            }
            if upload.ended
                || upload.pending.is_some()
                || upload.last_id.checked_add(1) != Some(request.request_id)
            {
                return Err(Status::failed_precondition(
                    "upload must be ordered and acknowledge each operation",
                ));
            }
            upload.last_id = request.request_id;
            let phase = match op {
                UploadOperation::Chunk(chunk) => {
                    upload
                        .update
                        .chunk(&mut *upload.target, chunk.offset, &chunk.data)
                        .map_err(upload_error)?;
                    self.env.event(UploadEvent::ChunkWritten {
                        request_id: request.request_id,
                        next_offset: upload.update.received(),
                    });
                    UploadPhase::Receiving
                }
                UploadOperation::Finish(_) => {
                    upload
                        .update
                        .finish(&mut *upload.target)
                        .map_err(upload_error)?;
                    upload.verified = true;
                    self.env.event(UploadEvent::Verified);
                    UploadPhase::Verified
                }
                UploadOperation::Abort(_) => {
                    upload.ended = true;
                    UploadPhase::Aborted
                }
                UploadOperation::Commit(_) => {
                    if !upload.verified {
                        return Err(Status::failed_precondition(
                            "image must be verified before commit",
                        ));
                    }
                    // A failed metadata operation or lost ACK can still leave boot
                    // selection changed. Block all further writes until reboot.
                    self.activation_pending = true;
                    self.env.event(UploadEvent::ActivationPending);
                    upload
                        .update
                        .commit(&mut *upload.target)
                        .map_err(upload_error)?;
                    upload.ended = true;
                    self.env.event(UploadEvent::Committed);
                    UploadPhase::Committed
                }
                _ => return Err(Status::invalid_argument("unexpected upload operation")),
            };
            (phase, upload.update.received())
        } else {
            let UploadOperation::Begin(begin) = op else {
                return Err(Status::failed_precondition("upload requires Begin"));
            };
            let target = self
                .target
                .try_borrow_mut()
                .map_err(|_| Status::resource_exhausted("flash busy"))?;
            let update = ota_core::Update::begin(
                &*target,
                &begin.application,
                &begin.chip,
                &begin.version,
                begin.image_size,
                &begin.sha256,
            )
            .map_err(upload_error)?;
            self.upload = Some(Upload {
                id: ctx.id,
                last_id: request.request_id,
                target,
                update,
                pending: None,
                ended: false,
                verified: false,
            });
            self.env.event(UploadEvent::Receiving);
            (UploadPhase::Receiving, 0)
        };
        let response = UploadAck {
            request_id: request.request_id,
            phase,
            next_offset: offset,
            target_offset: self.target_offset,
        };
        let pending = encode(&response)?;
        let upload = self
            .upload
            .as_mut()
            .ok_or_else(|| Status::internal("missing upload state"))?;
        upload.pending = Some(pending);
        Ok(())
    }
}
fn upload_error(error: ota_core::Error) -> Status {
    match error {
        ota_core::Error::Storage => Status::internal("flash operation failed"),
        ota_core::Error::Hash | ota_core::Error::Image => {
            Status::invalid_argument("image verification failed")
        }
        _ => Status::failed_precondition("invalid upload metadata, state, bounds or order"),
    }
}
fn encode(response: &impl ota_protocol::MessageEncode) -> Result<Vec<u8>, Status> {
    ota_protocol::encode(response)
        .map(|b| b.to_vec())
        .map_err(|_| Status::internal("response encoding failed"))
}
impl<T: ota_core::Target, E: Environment> Handler for StreamingManagement<'_, T, E> {
    fn route(&self, path: &str) -> Option<Route> {
        match path {
            STATUS_PATH => Some(Route::new(MethodKind::Unary, 0)),
            READ_LOGS_PATH => Some(Route::new(MethodKind::Unary, 3)),
            LOGS_PATH => Some(Route::new(MethodKind::ServerStreaming, 1)),
            UPLOAD_PATH => Some(Route::new(MethodKind::BidiStreaming, 2)),
            _ => None,
        }
    }
    fn call(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        if ctx.route.method == 3 {
            let cursor = ota_protocol::decode(request)
                .map_err(|_| Status::invalid_argument("invalid log cursor"))?;
            return encode(&self.env.read_logs(&cursor));
        }
        status_response(request, || self.env.status(), || self.env.now_us())
    }
    fn on_message(&mut self, ctx: &mut CallContext<'_>, message: &[u8]) -> Result<(), Status> {
        let result = self.message(ctx, message);
        if result.is_err() {
            self.on_cancel(ctx);
        }
        result
    }
    fn on_half_close(&mut self, ctx: &mut CallContext<'_>) -> Result<(), Status> {
        if ctx.route.method == 2 {
            let upload = self
                .upload
                .as_mut()
                .filter(|u| u.id == ctx.id)
                .ok_or_else(|| Status::failed_precondition("missing Begin"))?;
            upload.ended = true;
        }
        Ok(())
    }
    fn poll_response(
        &mut self,
        ctx: &mut CallContext<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<Next<Vec<u8>>> {
        if ctx.route.method == 1 {
            let Some(sub) = self
                .subscriptions
                .iter_mut()
                .flatten()
                .find(|s| s.id == ctx.id)
            else {
                return Poll::Ready(Next::Done(Err(Status::internal("missing subscription"))));
            };
            let batch = self.env.logs(&sub.cursor, cx);
            if sub.first || batch.reset || batch.dropped || !batch.entries.is_empty() {
                sub.first = false;
                sub.cursor.boot_id = batch.boot_id;
                sub.cursor.after_sequence = batch.next_sequence;
                return Poll::Ready(match encode(&batch) {
                    Ok(bytes) => Next::Message(bytes),
                    Err(e) => Next::Done(Err(e)),
                });
            }
            return Poll::Pending;
        }
        if let Some(upload) = self.upload.as_mut().filter(|u| u.id == ctx.id) {
            if let Some(bytes) = upload.pending.take() {
                return Poll::Ready(Next::Message(bytes));
            }
            if !upload.ended {
                return Poll::Pending;
            }
            self.release();
            return Poll::Ready(Next::Done(Ok(())));
        }
        Poll::Pending
    }
    fn on_cancel(&mut self, ctx: &mut CallContext<'_>) {
        for slot in &mut self.subscriptions {
            if slot.as_ref().is_some_and(|s| s.id == ctx.id) {
                *slot = None;
            }
        }
        if self.upload.as_ref().is_some_and(|u| u.id == ctx.id) {
            self.release();
        }
    }
}
impl<T: ota_core::Target, E: Environment> Drop for StreamingManagement<'_, T, E> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use core::cell::Cell;
    fn current() -> StatusResponse {
        let mut status = ota_protocol::status("mock", "device-demo", "v1", 1).unwrap();
        status.set_boot(ota_protocol::BootInfo::default());
        status
    }
    #[test]
    fn status_honors_boot_and_timing_flags() {
        for include in [false, true] {
            let request = ota_protocol::StatusRequest {
                include_boot: include,
                include_timings: include,
            };
            let clock = Cell::new(0u64);
            let response =
                status_response(&ota_protocol::encode(&request).unwrap(), current, || {
                    let n = clock.get();
                    clock.set(n + 5);
                    n
                })
                .unwrap();
            let status: StatusResponse = ota_protocol::decode(&response).unwrap();
            assert_eq!(status.boot().is_some(), include);
            assert_eq!(status.timings().is_some(), include);
            if let Some(t) = status.timings() {
                assert_eq!(t.decode_us, 5);
                assert_eq!(t.prepare_us, 5);
            }
        }
    }
}
