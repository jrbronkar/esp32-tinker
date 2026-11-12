/// A failure to decode a frame.
///
/// Generic parameters preserve the source and destination error types.
/// Match their variants to inspect them: the current [`core::error::Error`]
/// implementation does not forward `source()`.
///
/// Errors do not roll back partial output.
/// Only [`InvalidFrame`](Self::InvalidFrame) carries progress information.
///
/// Implementations such as `Clone`, `Copy`, serialization, and compact formatting
/// depend on the corresponding traits being implemented by both generic error parameters.
///
/// Error formatting is diagnostic output, not a stable serialization format.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DecodeError<SourceError, DestinationError> {
    /// Reading the source failed.
    ///
    /// Never returned by the [`sync`](crate::sync) module, which reads from
    /// memory.
    Source(SourceError),

    /// The destination cannot hold the next decoded byte.
    ///
    /// [`sync::decode_to_slice`](crate::sync::decode_to_slice) reports
    /// [`SeekableError::OutOfBounds`] here. The destination may be partially
    /// overwritten.
    Destination(DestinationError),

    /// The input ended before the current COBS block was complete.
    UnexpectedSourceEof,

    /// One-shot decoding reached EOF without starting an encoded frame.
    ///
    /// Empty input and zero padding alone produce this error. A valid
    /// encoding of an empty payload, such as `[1]`, does not.
    EmptyFrame,

    /// A zero delimiter arrived before the current COBS block had enough data.
    ///
    /// Progress includes the delimiter and has `frame_len: None`. Partial
    /// output already written to the destination is not removed.
    ///
    /// The next frame starts at `source[progress.consumed as usize..]`.
    InvalidFrame(crate::DecodeProgress),
}

impl<S: core::error::Error, D: core::error::Error> core::error::Error for DecodeError<S, D> {}

impl<S, D> core::fmt::Display for DecodeError<S, D>
where
    S: core::error::Error,
    D: core::error::Error,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Source(e) => write!(f, "Source error: {}", e),
            DecodeError::Destination(e) => write!(f, "Destination error: {}", e),
            DecodeError::UnexpectedSourceEof => write!(f, "Unexpected source EOF"),
            DecodeError::EmptyFrame => write!(f, "Empty frame"),
            DecodeError::InvalidFrame(progress) => {
                write!(f, "Invalid frame after decoding {:?}", progress)
            }
        }
    }
}

#[cfg(feature = "defmt")]
impl<S, D> defmt::Format for DecodeError<S, D>
where
    D: defmt::Format,
    S: defmt::Format,
{
    fn format(&self, f: defmt::Formatter<'_>) {
        match self {
            DecodeError::Source(e) => defmt::write!(f, "Source error: {}", e),
            DecodeError::Destination(e) => defmt::write!(f, "Destination error: {}", e),
            DecodeError::UnexpectedSourceEof => defmt::write!(f, "Unexpected source EOF"),
            DecodeError::EmptyFrame => defmt::write!(f, "Empty frame"),
            DecodeError::InvalidFrame(progress) => {
                defmt::write!(f, "Invalid frame after decoding {:?}", progress)
            }
        }
    }
}

/// A bounds error involving a slice-backed buffer.
///
/// The [`sync`](crate::sync) module returns this when the destination slice
/// cannot hold the output.
///
/// Implements [`core::error::Error`].
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SeekableError {
    /// An operation would access a position or write bytes outside the
    /// backing slice's supported bounds.
    OutOfBounds,
}

impl core::error::Error for SeekableError {}
impl core::fmt::Display for SeekableError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SeekableError::OutOfBounds => write!(f, "Out of bounds"),
        }
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for SeekableError {
    fn format(&self, f: defmt::Formatter<'_>) {
        match self {
            SeekableError::OutOfBounds => defmt::write!(f, "Out of bounds"),
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
/// An invalid [`Reliable`](crate::Reliable) configuration, rejected at
/// construction.
pub enum ConfigError {
    /// The buffer capacity `N` cannot hold a packet with one payload byte.
    ///
    /// A packet needs 17 bytes of header and checksum, COBS overhead and two
    /// delimiters, so `N` must be at least 21.
    BufferTooSmall,
}

impl core::error::Error for ConfigError {}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConfigError::BufferTooSmall => write!(f, "Buffer cannot hold a protocol packet"),
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
/// A terminal failure of a [`Reliable`](crate::Reliable) connection.
///
/// Corrupt or malformed frames never produce an error; they are discarded and
/// recovered by retransmission. Once any of these occurs, the connection is
/// permanently failed.
pub enum ConnectionError<TransportError> {
    /// The underlying transport returned an error.
    Transport(TransportError),

    /// The underlying transport reported end of stream.
    UnexpectedEof,

    /// The underlying transport accepted zero bytes of a nonempty write.
    WriteZero,

    /// A packet went unacknowledged after the configured number of
    /// retransmissions.
    Timeout,

    /// All 2^32 sequence numbers of this session have been used.
    ///
    /// Start a new session with a fresh identifier.
    SequenceExhausted,

    /// The connection failed earlier and the original error was already
    /// reported.
    Failed,
}

impl<E: core::error::Error> core::error::Error for ConnectionError<E> {}

impl<E: core::fmt::Display> core::fmt::Display for ConnectionError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConnectionError::Transport(e) => write!(f, "Transport error: {}", e),
            ConnectionError::UnexpectedEof => write!(f, "Transport closed unexpectedly"),
            ConnectionError::WriteZero => write!(f, "Transport accepted zero bytes"),
            ConnectionError::Timeout => write!(f, "Retransmission attempts exhausted"),
            ConnectionError::SequenceExhausted => write!(f, "Sequence numbers exhausted"),
            ConnectionError::Failed => write!(f, "Connection previously failed"),
        }
    }
}
