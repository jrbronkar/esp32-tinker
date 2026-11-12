//! Dispatch the three management RPCs through the device's protobuf types.

use alloc::vec::Vec;
use core::task::{Context, Poll};

use ota_protocol::{
    LOGS_PATH, MAX_FRAME, ReadLogsRequest, ReadLogsResponse, STATUS_PATH, StatusRequest,
    StatusResponse, UPLOAD_PATH, UploadAck, UploadRequest,
};
use protolink::{
    CallContext, Handler, MethodKind, Next, Route, Status,
    grpc::codec::{self, Micropb},
};

const LOGS: u32 = 0;
const UPLOAD: u32 = 1;
const STATUS: u32 = 2;

pub(crate) trait ManagementRpc {
    fn status(
        &mut self,
        ctx: &mut CallContext<'_>,
        request: StatusRequest,
    ) -> Result<StatusResponse, Status>;

    fn logs(&mut self, _: &mut CallContext<'_>, _: ReadLogsRequest) -> Result<(), Status> {
        Err(Status::unimplemented("`Logs` is not implemented"))
    }

    fn poll_logs(
        &mut self,
        _: &mut CallContext<'_>,
        _: &mut Context<'_>,
    ) -> Poll<Next<ReadLogsResponse>> {
        Poll::Ready(Next::Done(Err(Status::unimplemented(
            "`Logs` is not implemented",
        ))))
    }

    fn cancel_logs(&mut self, _: &mut CallContext<'_>) {}

    fn upload(&mut self, _: &mut CallContext<'_>, _: UploadRequest) -> Result<(), Status> {
        Err(Status::unimplemented("`Upload` is not implemented"))
    }

    fn end_upload(&mut self, _: &mut CallContext<'_>) -> Result<(), Status> {
        Ok(())
    }

    fn poll_upload(
        &mut self,
        _: &mut CallContext<'_>,
        _: &mut Context<'_>,
    ) -> Poll<Next<UploadAck>> {
        Poll::Ready(Next::Done(Err(Status::unimplemented(
            "`Upload` is not implemented",
        ))))
    }

    fn cancel_upload(&mut self, _: &mut CallContext<'_>) {}
}

/// Apply per-method request bounds and release streaming state on dispatch errors.
pub struct ManagementServer<S>(S);

impl<S> ManagementServer<S> {
    pub fn new(service: S) -> Self {
        Self(service)
    }
}

impl<S: ManagementRpc> Handler for ManagementServer<S> {
    fn route(&self, path: &str) -> Option<Route> {
        match path {
            LOGS_PATH => Some(Route::new(MethodKind::ServerStreaming, LOGS)),
            UPLOAD_PATH => Some(Route::new(MethodKind::BidiStreaming, UPLOAD)),
            STATUS_PATH => Some(Route::new(MethodKind::Unary, STATUS)),
            _ => None,
        }
    }

    fn call(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<Vec<u8>, Status> {
        if request.len() > MAX_FRAME {
            return Err(Status::invalid_argument(
                "status request exceeds message limit",
            ));
        }
        if ctx.route.method != STATUS {
            return Err(Status::unimplemented("unknown method"));
        }
        codec::unary::<Micropb, _, _>(request, |request| self.0.status(ctx, request))
    }

    fn on_message(&mut self, ctx: &mut CallContext<'_>, request: &[u8]) -> Result<(), Status> {
        let result = match ctx.route.method {
            LOGS if request.len() > MAX_FRAME => Err(Status::invalid_argument(
                "log request exceeds message limit",
            )),
            LOGS => codec::message::<Micropb, _>(request, |request| self.0.logs(ctx, request)),
            UPLOAD => codec::message::<Micropb, _>(request, |request| self.0.upload(ctx, request)),
            _ => Err(Status::unimplemented("unknown method")),
        };
        if result.is_err() {
            // Protolink finishes calls whose dispatch returns an error without
            // invoking on_cancel. Decode failures also need to release flash.
            self.on_cancel(ctx);
        }
        result
    }

    fn on_half_close(&mut self, ctx: &mut CallContext<'_>) -> Result<(), Status> {
        if ctx.route.method == UPLOAD {
            self.0.end_upload(ctx)
        } else {
            Ok(())
        }
    }

    fn poll_response(
        &mut self,
        ctx: &mut CallContext<'_>,
        cx: &mut Context<'_>,
    ) -> Poll<Next<Vec<u8>>> {
        match ctx.route.method {
            LOGS => codec::poll_stream::<Micropb, _>(self.0.poll_logs(ctx, cx)),
            UPLOAD => codec::poll_stream::<Micropb, _>(self.0.poll_upload(ctx, cx)),
            _ => Poll::Ready(Next::Done(Err(Status::unimplemented("unknown method")))),
        }
    }

    fn on_cancel(&mut self, ctx: &mut CallContext<'_>) {
        match ctx.route.method {
            LOGS => self.0.cancel_logs(ctx),
            UPLOAD => self.0.cancel_upload(ctx),
            _ => {}
        }
    }
}
