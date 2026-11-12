#![no_std]
#![forbid(unsafe_code)]

//! Generated embedded Management service bindings for unary RPCs.

pub use embedded_grpc::Status;

include!(concat!(env!("OUT_DIR"), "/management.rs"));
