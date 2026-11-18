#![no_std]

//! Build-time settings compiled from the selected firmware YAML file.

use core::net::{Ipv4Addr, SocketAddrV4};

pub struct MqttSettings {
    pub broker: SocketAddrV4,
    pub device_id: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/settings.rs"));
