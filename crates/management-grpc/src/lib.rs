#![no_std]
#![forbid(unsafe_code)]

//! Generated embedded Management service bindings for unary, server-streaming, and bidirectional RPCs.
//! Cargo generates the service trait and dispatch from `protocol/management.proto`.
//! Firmware implements the generated trait to provide device behavior.

pub use embedded_grpc::Status;

include!(concat!(env!("OUT_DIR"), "/management.rs"));
