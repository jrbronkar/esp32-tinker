mod connection;
mod protocol;
mod support;
mod sync;

#[test]
fn public_value_types_support_copy_equality_and_hashing() {
    use crate::{Config, ConfigError, ConnectionError, DecodeError, DecodeProgress, SeekableError};

    fn assert_traits<T: Copy + Eq + core::hash::Hash>() {}

    assert_traits::<SeekableError>();
    assert_traits::<DecodeProgress>();
    assert_traits::<DecodeError<SeekableError, SeekableError>>();
    assert_traits::<Config>();
    assert_traits::<ConfigError>();
    assert_traits::<ConnectionError<SeekableError>>();
}
