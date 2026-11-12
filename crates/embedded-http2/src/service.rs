//! Buffered HTTP exchanges over the single-stream HTTP/2 connection.

use alloc::vec::Vec;
use embedded_io_async::{Read, Write};

use crate::{Error, HeaderField, Server};

pub struct Request {
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
    pub trailers: Vec<HeaderField>,
}

pub struct Response {
    pub headers: Vec<HeaderField>,
    pub body: Vec<u8>,
    pub trailers: Vec<HeaderField>,
}

/// Handles one complete request at a time. The caller supplies the executor.
#[allow(async_fn_in_trait)]
pub trait Service {
    async fn call(&mut self, request: Request) -> Response;
}

impl<Io: Read + Write> Server<Io> {
    /// Serve sequential exchanges until the connection closes or fails.
    ///
    /// A reset observed while receiving or sending an exchange is recoverable.
    /// The buffered handler runs to completion before the connection reads again.
    pub async fn serve<S: Service>(&mut self, service: &mut S) -> Result<(), Error<Io::Error>> {
        loop {
            let request = match self.accept().await {
                Ok(request) => request,
                Err(Error::Reset { .. }) => continue,
                Err(error) => return Err(error),
            };
            let response = service
                .call(Request {
                    headers: request.headers,
                    body: request.body,
                    trailers: request.trailers,
                })
                .await;
            match self
                .respond(
                    request.stream_id,
                    crate::Response {
                        headers: &response.headers,
                        body: &response.body,
                        trailers: &response.trailers,
                    },
                )
                .await
            {
                Ok(()) | Err(Error::Reset { .. }) => {}
                Err(error) => return Err(error),
            }
        }
    }
}
