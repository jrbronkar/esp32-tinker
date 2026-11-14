#![no_std]
extern crate alloc;
mod embedded;
pub use embedded::EmbeddedManagement;
pub mod logs;
pub mod watchdog;
use core::{
    cell::{RefCell, RefMut},
    task::Context,
};
use embedded_grpc::Status;
pub use ota_protocol::{LOGS_PATH, STATUS_PATH, UPLOAD_PATH};
use ota_protocol::{
    ReadLogsRequest, ReadLogsResponse, StatusRequest, StatusResponse, UploadAck, UploadOperation,
    UploadPhase, UploadRequest,
};

pub const PORT: u16 = ota_protocol::PORT;

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
struct Upload<'a, T> {
    last_id: u32,
    target: RefMut<'a, T>,
    update: ota_core::Update,
    verified: bool,
}
/// State for one Upload RPC and its exclusive flash lease.
/// Drop the session on cancellation or after a terminal acknowledgement.
pub struct UploadSession<'a, T: ota_core::Target, E: Environment> {
    target: &'a RefCell<T>,
    target_offset: u32,
    activation_pending: bool,
    env: E,
    upload: Option<Upload<'a, T>>,
}
impl<'a, T: ota_core::Target, E: Environment> UploadSession<'a, T, E> {
    pub fn new(target: &'a RefCell<T>, target_offset: u32, env: E) -> Self {
        Self {
            target,
            target_offset,
            activation_pending: false,
            env,
            upload: None,
        }
    }
    fn release(&mut self) {
        if self.upload.take().is_some() && !self.activation_pending {
            self.env.event(UploadEvent::Released);
        }
    }
    pub fn process(&mut self, request: UploadRequest) -> Result<UploadAck, Status> {
        if self.activation_pending || self.env.activation_pending() {
            return Err(Status::FailedPrecondition);
        }
        let op = request.payload.ok_or(Status::InvalidArgument)?;
        let (phase, offset) = if let Some(upload) = self.upload.as_mut() {
            if upload.last_id.checked_add(1) != Some(request.request_id) {
                return Err(Status::FailedPrecondition);
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
                UploadOperation::Abort(_) => UploadPhase::Aborted,
                UploadOperation::Commit(_) => {
                    if !upload.verified {
                        return Err(Status::FailedPrecondition);
                    }
                    // A failed metadata operation or lost ACK can still leave boot
                    // selection changed. Block all further writes until reboot.
                    self.activation_pending = true;
                    self.env.event(UploadEvent::ActivationPending);
                    upload
                        .update
                        .commit(&mut *upload.target)
                        .map_err(upload_error)?;
                    self.env.event(UploadEvent::Committed);
                    UploadPhase::Committed
                }
                _ => return Err(Status::InvalidArgument),
            };
            (phase, upload.update.received())
        } else {
            let UploadOperation::Begin(begin) = op else {
                return Err(Status::FailedPrecondition);
            };
            if !self.env.accepts_application(&begin.application) {
                return Err(Status::FailedPrecondition);
            }
            let target = self
                .target
                .try_borrow_mut()
                .map_err(|_| Status::ResourceExhausted)?;
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
                last_id: request.request_id,
                target,
                update,
                verified: false,
            });
            self.env.event(UploadEvent::Receiving);
            (UploadPhase::Receiving, 0)
        };
        Ok(UploadAck {
            request_id: request.request_id,
            phase,
            next_offset: offset,
            target_offset: self.target_offset,
        })
    }
}
fn upload_error(error: ota_core::Error) -> Status {
    match error {
        ota_core::Error::Storage => Status::Internal,
        ota_core::Error::Hash | ota_core::Error::Image => Status::InvalidArgument,
        _ => Status::FailedPrecondition,
    }
}
impl<T: ota_core::Target, E: Environment> Drop for UploadSession<'_, T, E> {
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
