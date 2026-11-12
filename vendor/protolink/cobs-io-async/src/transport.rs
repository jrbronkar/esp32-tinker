use core::task::{Context, Poll};

/// A poll-based, runtime-independent byte-stream interface.
///
/// Implement this for the link underneath a [`Reliable`](crate::Reliable)
/// connection. [`Reliable`](crate::Reliable) implements it too, so the
/// reliable stream can be used wherever a byte stream is expected.
///
/// Each method follows the usual poll contract: when it returns
/// [`Poll::Pending`], it has arranged for `cx.waker()` to be woken when the
/// call could make progress. Methods take `&mut self` and impose no `Send`,
/// `'static` or pinning requirements; runtime adapters handle those
/// themselves.
pub trait Transport {
    /// The error produced by the underlying link.
    type Error;

    /// Attempts to read bytes into `buf`, returning the number read.
    ///
    /// For a nonempty `buf`, `Ready(Ok(0))` means end of stream. Temporary
    /// lack of input must be reported as `Pending`.
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Self::Error>>;

    /// Attempts to write a prefix of `buf`, returning the number of bytes
    /// accepted.
    ///
    /// Accepted bytes need not have been transmitted yet; use
    /// [`poll_flush`](Self::poll_flush) for that. For a nonempty `buf`,
    /// `Ready(Ok(0))` is treated as an error by [`Reliable`](crate::Reliable).
    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Self::Error>>;

    /// Attempts to push all previously accepted bytes to the peer.
    ///
    /// Completes with `Ready(Ok(()))` once nothing remains buffered.
    fn poll_flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>;
}
