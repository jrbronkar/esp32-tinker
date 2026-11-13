#![no_std]
extern crate alloc;
pub mod logs;
pub mod watchdog;
pub use ota_protocol::{LOGS_PATH, STATUS_PATH, UPLOAD_PATH};
pub use protolink;

use alloc::vec::Vec;
use core::{
    cell::{RefCell, RefMut},
    task::{Context, Poll},
};
use ota_protocol::{
    ReadLogsRequest, ReadLogsResponse, StatusRequest, StatusResponse, UploadAck, UploadOperation,
    UploadPhase, UploadRequest,
};
use protolink::{CallContext, CallId, Handler, Next, Route, ServerConfig, Status};

mod bindings {
    use ota_protocol::ota_;

    include!(concat!(env!("OUT_DIR"), "/management.rs"));
}

use bindings::management::{Management as ManagementRpc, ManagementDispatch};

pub const PORT: u16 = ota_protocol::PORT;

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
impl<F: FnMut() -> StatusResponse> ManagementRpc for Management<F> {
    fn status(
        &mut self,
        _: &mut CallContext<'_>,
        request: StatusRequest,
    ) -> Result<StatusResponse, Status> {
        Ok(status_response(request, || (self.0)(), || 0))
    }
}

fn status_response(
    options: StatusRequest,
    status: impl FnOnce() -> StatusResponse,
    now: impl Fn() -> u64,
) -> StatusResponse {
    let started = now();
    let mut current = status();
    if !options.include_boot {
        current.clear_boot();
    }
    if options.include_timings {
        current.set_timings(ota_protocol::DeviceTimings {
            // Protobuf decoding completes before the typed handler runs.
            decode_us: 0,
            prepare_us: now().saturating_sub(started),
        });
    } else {
        current.clear_timings();
    }
    current
}

/// Apply device request bounds and release streaming state on dispatch errors.
pub struct ManagementServer<S>(ManagementDispatch<S>);

impl<S: ManagementRpc> ManagementServer<S> {
    pub fn new(service: S) -> Self {
        Self(ManagementDispatch::new(service))
    }
}

impl<S: ManagementRpc> Handler for ManagementServer<S> {
    fn route(&self, path: &str) -> Option<Route> {
        self.0.route(path)
    }

    fn call(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        if request.len() > ota_protocol::MAX_FRAME {
            return Err(Status::invalid_argument(
                "status request exceeds message limit",
            ));
        }
        self.0.call(ctx, request)
    }

    fn on_message(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<(), Status> {
        let result = if ctx.path == LOGS_PATH && request.len() > ota_protocol::MAX_FRAME {
            Err(Status::invalid_argument(
                "log request exceeds message limit",
            ))
        } else {
            self.0.on_message(ctx, request)
        };
        if result.is_err() {
            // Protolink finishes calls whose dispatch returns an error without
            // invoking on_cancel. Decode failures also need to release flash.
            self.0.on_cancel(ctx);
        }
        result
    }

    fn on_half_close(&mut self, ctx: &mut CallContext<'_>) -> Result<(), Status> {
        self.0.on_half_close(ctx)
    }

    fn poll_response(
        &mut self,
        ctx: &mut CallContext<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<Next<Vec<u8>>> {
        self.0.poll_response(ctx, cx)
    }

    fn on_cancel(&mut self, ctx: &mut CallContext<'_>) {
        self.0.on_cancel(ctx);
    }
}

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
    fn accepts_application(&self, application: &str) -> bool {
        self.status().application.as_str() == application
    }
    fn logs(&self, cursor: &ReadLogsRequest, cx: &mut Context<'_>) -> ReadLogsResponse;
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
    cursor: logs::LogCursor,
}
struct Upload<'a, T> {
    id: CallId,
    last_id: u32,
    target: RefMut<'a, T>,
    update: ota_core::Update,
    pending: Option<UploadAck>,
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
    fn process_upload(
        &mut self,
        ctx: &mut CallContext<'_>,
        request: UploadRequest,
    ) -> Result<(), Status> {
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
            if !self.env.accepts_application(&begin.application) {
                return Err(Status::failed_precondition(
                    "image application does not match this board",
                ));
            }
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
        let upload = self
            .upload
            .as_mut()
            .ok_or_else(|| Status::internal("missing upload state"))?;
        upload.pending = Some(response);
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
impl<T: ota_core::Target, E: Environment> ManagementRpc for StreamingManagement<'_, T, E> {
    fn status(
        &mut self,
        _: &mut CallContext<'_>,
        request: StatusRequest,
    ) -> Result<StatusResponse, Status> {
        Ok(status_response(
            request,
            || self.env.status(),
            || self.env.now_us(),
        ))
    }

    fn logs(&mut self, ctx: &mut CallContext<'_>, cursor: ReadLogsRequest) -> Result<(), Status> {
        let slot = self
            .subscriptions
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| Status::resource_exhausted("too many subscriptions"))?;
        *slot = Some(Subscription {
            id: ctx.id,
            cursor: logs::LogCursor::new(cursor),
        });
        Ok(())
    }

    fn poll_logs(
        &mut self,
        ctx: &mut CallContext<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<Next<ReadLogsResponse>> {
        let Some(sub) = self
            .subscriptions
            .iter_mut()
            .flatten()
            .find(|sub| sub.id == ctx.id)
        else {
            return Poll::Ready(Next::Done(Err(Status::internal("missing subscription"))));
        };
        sub.cursor
            .poll(|cursor| self.env.logs(cursor, cx))
            .map(Next::Message)
    }

    fn cancel_logs(&mut self, ctx: &mut CallContext<'_>) {
        for slot in &mut self.subscriptions {
            if slot.as_ref().is_some_and(|sub| sub.id == ctx.id) {
                *slot = None;
            }
        }
    }

    fn upload(&mut self, ctx: &mut CallContext<'_>, request: UploadRequest) -> Result<(), Status> {
        self.process_upload(ctx, request)
    }

    fn end_upload(&mut self, ctx: &mut CallContext<'_>) -> Result<(), Status> {
        let upload = self
            .upload
            .as_mut()
            .filter(|upload| upload.id == ctx.id)
            .ok_or_else(|| Status::failed_precondition("missing Begin"))?;
        upload.ended = true;
        Ok(())
    }

    fn poll_upload(
        &mut self,
        ctx: &mut CallContext<'_>,
        _: &mut Context<'_>,
    ) -> Poll<Next<UploadAck>> {
        if let Some(upload) = self.upload.as_mut().filter(|upload| upload.id == ctx.id) {
            if let Some(ack) = upload.pending.take() {
                return Poll::Ready(Next::Message(ack));
            }
            if upload.ended {
                self.release();
                return Poll::Ready(Next::Done(Ok(())));
            }
        }
        Poll::Pending
    }

    fn cancel_upload(&mut self, ctx: &mut CallContext<'_>) {
        if self
            .upload
            .as_ref()
            .is_some_and(|upload| upload.id == ctx.id)
        {
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
            let status = status_response(request, current, || {
                let n = clock.get();
                clock.set(n + 5);
                n
            });
            assert_eq!(status.boot().is_some(), include);
            assert_eq!(status.timings().is_some(), include);
            if let Some(t) = status.timings() {
                assert_eq!(t.decode_us, 0);
                assert_eq!(t.prepare_us, 5);
            }
        }
    }
}
