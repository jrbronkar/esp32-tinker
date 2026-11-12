use core::task::{Context, Poll};

/// A monotonic clock and wakeup source for retransmission deadlines.
///
/// Reading a timestamp alone is not enough: a lost packet must cause the
/// connection to be polled again when its deadline arrives, so a timer also
/// registers wakeups. Implement this for your platform's clock and timer
/// facility.
///
/// All values are `u64` ticks of a unit you choose, for example
/// milliseconds. [`Config::retransmit_timeout`](crate::Config::retransmit_timeout)
/// uses the same unit.
pub trait Timer {
    /// Returns the current time in ticks.
    ///
    /// Must be monotonic: successive calls never return a smaller value.
    fn now(&self) -> u64;

    /// Returns `Ready` once [`now`](Self::now) has reached `deadline`.
    ///
    /// Otherwise returns `Pending` after arranging for `cx.waker()` to be
    /// woken no later than the deadline. Deadlines are computed with
    /// saturating addition, so `u64::MAX` means "never".
    fn poll_deadline(&mut self, cx: &mut Context<'_>, deadline: u64) -> Poll<()>;
}
