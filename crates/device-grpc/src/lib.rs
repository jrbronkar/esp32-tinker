#![no_std]
extern crate alloc;
pub mod watchdog;
pub use protolink;

use alloc::vec::Vec;
use ota_protocol::{Request, RequestPayload, Response, ResponsePayload, StatusResponse};
use protolink::{CallContext, Handler, MethodKind, Route, ServerConfig, Status};

pub const PORT: u16 = 7780;
pub const STATUS_PATH: &str = "/esp.experimental.Management/Status";

/// Small prototype limits, not a measured peak heap guarantee.
pub fn config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.http2.max_concurrent_streams = 2;
    config.http2.initial_window_size = 8192;
    // HTTP/2 cannot reduce the initial connection window below 65535.
    config.http2.connection_window_size = 65_535;
    config.http2.max_header_list_size = 1024;
    config.http2.max_pending_output_bytes = 8192;
    config.max_message_size = ota_protocol::UPLOAD_FRAME;
    config
}

/// Raw protobuf handler keeps the existing micropb 0.7 messages independent of
/// Protolink's optional micropb 0.6 codec. No legacy length prefix is sent.
pub struct Management<F>(pub F);
impl<F: FnMut() -> StatusResponse> Handler for Management<F> {
    fn route(&self, path: &str) -> Option<Route> {
        (path == STATUS_PATH).then_some(Route::new(MethodKind::Unary, 0))
    }
    fn call(&mut self, _: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        let request: Request = ota_protocol::decode(request)
            .map_err(|_| Status::invalid_argument("invalid management request"))?;
        if request.protocol_version != ota_protocol::VERSION
            || !matches!(request.payload, Some(RequestPayload::Status(_)))
        {
            return Err(Status::invalid_argument(
                "expected protocol v1 status request",
            ));
        }
        let response = Response {
            protocol_version: ota_protocol::VERSION,
            request_id: request.request_id,
            payload: Some(ResponsePayload::Status((self.0)())),
        };
        ota_protocol::encode(&response)
            .map(|bytes| bytes.as_slice().to_vec())
            .map_err(|_| Status::internal("status encoding failed"))
    }
}

use core::{
    cell::{RefCell, RefMut},
    task::{Context, Poll},
};
use ota_protocol::{ReadLogsRequest, ReadLogsResponse, UploadAck, UploadPhase};
use protolink::{CallId, Next};
pub const LOGS_PATH: &str = "/esp.experimental.Management/Logs";
pub const UPLOAD_PATH: &str = "/esp.experimental.Management/Upload";

/// External log producers must wake the registered task when records arrive.
pub trait Environment {
    fn status(&self) -> StatusResponse;
    fn logs(&self, cursor: &ReadLogsRequest, cx: &mut Context<'_>) -> ReadLogsResponse;
    fn event(&self, message: &str);
    fn activation_pending(&self) -> bool {
        false
    }
}
struct Subscription {
    id: CallId,
    request_id: u32,
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
            self.env.event("gRPC upload released");
        }
    }
    fn message(&mut self, ctx: &CallContext<'_>, bytes: &[u8]) -> Result<(), Status> {
        let request = ota_protocol::decode_upload(bytes)
            .map_err(|_| Status::invalid_argument("invalid protobuf"))?;
        if request.protocol_version != ota_protocol::VERSION {
            return Err(Status::invalid_argument("expected protocol v1"));
        }
        if ctx.route.method == 1 {
            let Some(RequestPayload::ReadLogs(cursor)) = request.payload else {
                return Err(Status::invalid_argument("expected log cursor"));
            };
            let slot = self
                .subscriptions
                .iter_mut()
                .find(|s| s.is_none())
                .ok_or_else(|| Status::resource_exhausted("too many subscriptions"))?;
            *slot = Some(Subscription {
                id: ctx.id,
                request_id: request.request_id,
                cursor,
                first: true,
            });
            return Ok(());
        }
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
                RequestPayload::Chunk(chunk) => {
                    upload
                        .update
                        .chunk(&mut *upload.target, chunk.offset, &chunk.data)
                        .map_err(upload_error)?;
                    self.env.event(&alloc::format!(
                        "gRPC chunk id={} next={}",
                        request.request_id,
                        upload.update.received()
                    ));
                    UploadPhase::Receiving
                }
                RequestPayload::Finish(_) => {
                    upload
                        .update
                        .finish(&mut *upload.target)
                        .map_err(upload_error)?;
                    upload.verified = true;
                    self.env.event("gRPC image verified");
                    UploadPhase::Verified
                }
                RequestPayload::Abort(_) => {
                    upload.ended = true;
                    UploadPhase::Aborted
                }
                RequestPayload::Commit(_) => {
                    if !upload.verified {
                        return Err(Status::failed_precondition(
                            "image must be verified before commit",
                        ));
                    }
                    // A failed metadata operation or lost ACK can still leave boot
                    // selection changed. Block all further writes until reboot.
                    self.activation_pending = true;
                    self.env.event("gRPC activation pending");
                    upload
                        .update
                        .commit(&mut *upload.target)
                        .map_err(upload_error)?;
                    upload.ended = true;
                    self.env.event("gRPC upload committed");
                    UploadPhase::Committed
                }
                _ => return Err(Status::invalid_argument("unexpected upload operation")),
            };
            (phase, upload.update.received())
        } else {
            let RequestPayload::Begin(begin) = op else {
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
            self.env.event("gRPC upload receiving");
            (UploadPhase::Receiving, 0)
        };
        let response = Response {
            protocol_version: ota_protocol::VERSION,
            request_id: request.request_id,
            payload: Some(ResponsePayload::Upload(UploadAck {
                phase,
                next_offset: offset,
                target_offset: self.target_offset,
            })),
        };
        self.upload.as_mut().unwrap().pending = Some(encode(&response)?);
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
fn encode(response: &Response) -> Result<Vec<u8>, Status> {
    ota_protocol::encode(response)
        .map(|b| b.to_vec())
        .map_err(|_| Status::internal("response encoding failed"))
}
impl<T: ota_core::Target, E: Environment> Handler for StreamingManagement<'_, T, E> {
    fn route(&self, path: &str) -> Option<Route> {
        match path {
            STATUS_PATH => Some(Route::new(MethodKind::Unary, 0)),
            LOGS_PATH => Some(Route::new(MethodKind::ServerStreaming, 1)),
            UPLOAD_PATH => Some(Route::new(MethodKind::BidiStreaming, 2)),
            _ => None,
        }
    }
    fn call(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        Management(|| self.env.status()).call(ctx, request)
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
                return Poll::Ready(
                    match encode(&Response {
                        protocol_version: ota_protocol::VERSION,
                        request_id: sub.request_id,
                        payload: Some(ResponsePayload::Logs(batch)),
                    }) {
                        Ok(bytes) => Next::Message(bytes),
                        Err(e) => Next::Done(Err(e)),
                    },
                );
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
