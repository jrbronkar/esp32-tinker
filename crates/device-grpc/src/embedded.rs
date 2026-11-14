//! Management handlers for the embedded gRPC transport.

use core::{
    cell::RefCell,
    pin::Pin,
    task::{Context, Poll},
};
use embedded_grpc::{Request, Response, Status, Stream, Streaming};
use ota_protocol::{
    ReadLogsRequest, ReadLogsResponse, StatusRequest, StatusResponse, UPLOAD_FRAME, UploadAck,
    UploadRequest,
};

use crate::{Environment, UploadSession, logs::LogCursor, status_response};

pub struct EmbeddedManagement<'a, T, E> {
    target: &'a RefCell<T>,
    target_offset: u32,
    env: E,
}

impl<'a, T, E> EmbeddedManagement<'a, T, E> {
    pub fn new(target: &'a RefCell<T>, target_offset: u32, env: E) -> Self {
        Self {
            target,
            target_offset,
            env,
        }
    }
}

impl<T: ota_core::Target, E: Environment> management_grpc::Management
    for EmbeddedManagement<'_, T, E>
{
    async fn status(&self, request: StatusRequest) -> Result<StatusResponse, Status> {
        Ok(status_response(
            request,
            || self.env.status(),
            || self.env.now_us(),
        ))
    }

    async fn logs(
        &self,
        request: ReadLogsRequest,
    ) -> Result<impl Stream<Item = Result<ReadLogsResponse, Status>>, Status> {
        Ok(Logs {
            cursor: LogCursor::new(request),
            env: &self.env,
        })
    }

    async fn upload(
        &self,
        request: Request<Streaming<UploadRequest, UPLOAD_FRAME>>,
    ) -> Result<Response<impl Stream<Item = Result<UploadAck, Status>>>, Status> {
        let inbound = request.into_inner();
        let session = UploadSession::new(self.target, self.target_offset, &self.env);
        Ok(Response::new(UploadReplies {
            inbound,
            session,
            ended: false,
        }))
    }
}

struct UploadReplies<'a, T: ota_core::Target, E: Environment> {
    inbound: Streaming<UploadRequest, UPLOAD_FRAME>,
    session: UploadSession<'a, T, &'a E>,
    ended: bool,
}

impl<T: ota_core::Target, E: Environment> Stream for UploadReplies<'_, T, E> {
    type Item = Result<UploadAck, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self {
            inbound,
            session,
            ended,
        } = self.get_mut();
        if *ended {
            return Poll::Ready(None);
        }
        let result = match core::task::ready!(Pin::new(inbound).poll_next(cx)) {
            Some(Ok(request)) => session.process(request).inspect(|ack| {
                *ended = matches!(
                    ack.phase,
                    ota_protocol::UploadPhase::Aborted | ota_protocol::UploadPhase::Committed
                );
            }),
            Some(Err(status)) => Err(status),
            None if session.upload.is_some() => return Poll::Ready(None),
            None => Err(Status::FailedPrecondition),
        };
        *ended |= result.is_err();
        Poll::Ready(Some(result))
    }
}

struct Logs<'a, E> {
    cursor: LogCursor,
    env: &'a E,
}

impl<E: Environment> Stream for Logs<'_, E> {
    type Item = Result<ReadLogsResponse, Status>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self { cursor, env } = self.get_mut();
        cursor
            .poll(|request| env.logs(request, cx))
            .map(|batch| Some(Ok(batch)))
    }
}

impl<E: Environment> Environment for &E {
    fn status(&self) -> StatusResponse {
        (*self).status()
    }
    fn accepts_application(&self, application: &str) -> bool {
        (*self).accepts_application(application)
    }
    fn logs(&self, cursor: &ReadLogsRequest, cx: &mut Context<'_>) -> ReadLogsResponse {
        (*self).logs(cursor, cx)
    }
    fn now_us(&self) -> u64 {
        (*self).now_us()
    }
    fn activation_pending(&self) -> bool {
        (*self).activation_pending()
    }
    fn event(&self, event: crate::UploadEvent) {
        (*self).event(event)
    }
}
