#![no_std]
extern crate alloc;
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
    config.http2.initial_window_size = 1024;
    // HTTP/2 cannot reduce the initial connection window below 65535.
    config.http2.connection_window_size = 65_535;
    config.http2.max_header_list_size = 1024;
    config.http2.max_pending_output_bytes = 4096;
    config.max_message_size = ota_protocol::MAX_FRAME;
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
