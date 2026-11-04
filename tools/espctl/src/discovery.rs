//! Bounded IPv4 LAN discovery. Responses are observations, not authenticated identity.
use ota_protocol::{Response, ResponsePayload, StatusResponse, MAX_FRAME, PORT, VERSION};
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_DEVICES: usize = 64;
#[derive(Debug)]
pub struct Device {
    pub address: SocketAddr,
    pub status: StatusResponse,
}
#[derive(Debug)]
pub struct Discovery {
    pub devices: Vec<Device>,
    pub limit_reached: bool,
}

/// Send probes every 500 ms until the collection window expires. No address scan.
/// `target` may be a subnet broadcast or loopback endpoint for tests.
pub fn discover(target: SocketAddr, bind: Ipv4Addr, window: Duration) -> io::Result<Discovery> {
    if !target.is_ipv4() || window.is_zero() || window > Duration::from_secs(60) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPv4 and a discovery window in 1..60000 ms are required",
        ));
    }
    let socket = UdpSocket::bind((bind, 0))?;
    socket.set_broadcast(true)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        ^ std::process::id();
    let request = ota_protocol::discovery_request(nonce);
    let body = ota_protocol::encode(&request)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "encode discovery"))?;
    let deadline = Instant::now() + window;
    let mut next_probe = Instant::now();
    let mut found = BTreeMap::new();
    let mut limit_reached = false;
    let mut packet = [0; MAX_FRAME + 1];
    while Instant::now() < deadline {
        let now = Instant::now();
        if now >= next_probe {
            socket.send_to(&body, target)?;
            next_probe = now + Duration::from_millis(500);
        }
        let wait = deadline
            .min(next_probe)
            .saturating_duration_since(Instant::now());
        socket.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        match socket.recv_from(&mut packet) {
            Ok((n, peer)) => {
                let std::net::IpAddr::V4(ip) = peer.ip() else {
                    continue;
                };
                if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
                    continue;
                }
                if peer.port() != target.port() {
                    continue;
                }
                let Ok(response) = ota_protocol::decode::<Response>(&packet[..n]) else {
                    continue;
                };
                if response.protocol_version != VERSION || response.request_id != request.request_id
                {
                    continue;
                }
                let Some(ResponsePayload::Status(status)) = response.payload else {
                    continue;
                };
                if status.device_id.is_empty()
                    || status.application.is_empty()
                    || status.chip.is_empty()
                    || status.firmware_version.is_empty()
                {
                    continue;
                }
                let address = SocketAddr::new(peer.ip(), PORT);
                let key = (status.device_id.to_string(), address);
                if found.contains_key(&key) || found.len() < MAX_DEVICES {
                    found.insert(key, Device { address, status });
                } else {
                    limit_reached = true;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Discovery {
        devices: found.into_values().collect(),
        limit_reached,
    })
}
