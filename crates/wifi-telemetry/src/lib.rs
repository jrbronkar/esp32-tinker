#![no_std]

use core::net::Ipv4Addr;
use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    Stack,
};
use esp_println::println;

const MAX_RECORD_BYTES: usize = 192;

pub struct UdpTelemetry {
    socket: UdpSocket<'static>,
    destination: (Ipv4Addr, u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    InvalidSettings,
    Socket,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    RecordTooLong,
    Network,
}

macro_rules! make_static {
    ($type:ty, $value:expr) => {{
        static CELL: static_cell::StaticCell<$type> = static_cell::StaticCell::new();
        CELL.uninit().write($value)
    }};
}

impl UdpTelemetry {
    /// Attach a single UDP telemetry sender to an already initialized network.
    pub fn new(stack: Stack<'static>, destination: Ipv4Addr, port: u16) -> Result<Self, InitError> {
        if port == 0 {
            return Err(InitError::InvalidSettings);
        }
        let rx_metadata = make_static!([PacketMetadata; 1], [PacketMetadata::EMPTY; 1]);
        let rx_buffer = make_static!([u8; 64], [0; 64]);
        let tx_metadata = make_static!([PacketMetadata; 2], [PacketMetadata::EMPTY; 2]);
        let tx_buffer = make_static!([u8; 256], [0; 256]);
        let mut socket = UdpSocket::new(stack, rx_metadata, rx_buffer, tx_metadata, tx_buffer);
        socket.bind(0).map_err(|_| InitError::Socket)?;
        println!("Sending telemetry to {destination}:{port}");
        Ok(Self {
            socket,
            destination: (destination, port),
        })
    }

    /// Sends one record as one UDP datagram, appending a newline for simple
    /// receivers such as `nc -u -l 7777`.
    pub async fn send_line(&mut self, record: &str) -> Result<(), SendError> {
        if record.len() + 1 > MAX_RECORD_BYTES {
            return Err(SendError::RecordTooLong);
        }

        let mut packet = [0_u8; MAX_RECORD_BYTES];
        packet[..record.len()].copy_from_slice(record.as_bytes());
        packet[record.len()] = b'\n';
        self.socket
            .send_to(&packet[..record.len() + 1], self.destination)
            .await
            .map_err(|_| SendError::Network)
    }
}
