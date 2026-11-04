//! Browse standard DNS-SD services; do not implement DNS or assume a management port.
use mdns_sd::{IfKind, ResolvedService, ServiceDaemon, ServiceEvent};
use ota_protocol::txt;
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

pub const MAX_DEVICES: usize = 64;
pub const MIN_SCAN_MS: u64 = 1;
pub const MAX_SCAN_MS: u64 = 60_000;
pub const DEFAULT_SCAN_MS: u64 = 3_000;
pub const WATCH_INTERVAL: Duration = Duration::from_secs(3);
const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ID_BYTES: usize = 64;
const MAX_FIRMWARE_BYTES: usize = 64;
const MAX_STATE_BYTES: usize = 16;
#[derive(Debug, Clone)]
pub struct Device {
    pub address: SocketAddr,
    pub upload_address: Option<SocketAddr>,
    pub device_id: String,
    pub firmware: String,
    pub management_state: String,
    pub ota_available: bool,
}
#[derive(Debug)]
pub struct Discovery {
    pub devices: Vec<Device>,
    pub limit_reached: bool,
}
#[derive(Default)]
struct Collector {
    devices: BTreeMap<(String, SocketAddr), Device>,
    limit_reached: bool,
}
impl Collector {
    fn event(&mut self, event: ServiceEvent) {
        match event {
            ServiceEvent::ServiceResolved(info) => self.resolved(&info),
            ServiceEvent::ServiceRemoved(_, name) => {
                self.devices.retain(|(instance, _), _| instance != &name)
            }
            _ => {}
        }
    }
    fn resolved(&mut self, info: &ResolvedService) {
        let name = info.get_fullname();
        // Replace the instance snapshot, including addresses withdrawn by a newer record.
        self.devices.retain(|(instance, _), _| instance != name);
        if info.ty_domain != ota_protocol::SERVICE_TYPE
            || info.get_port() == 0
            || info.get_property_val_str(txt::PROTOCOL) != Some(ota_protocol::VERSION_TXT)
        {
            return;
        }
        let Some(id) = info
            .get_property_val_str(txt::ID)
            .filter(|s| !s.is_empty() && s.len() <= MAX_ID_BYTES)
        else {
            return;
        };
        let Some(firmware) = info
            .get_property_val_str(txt::FIRMWARE)
            .filter(|s| !s.is_empty() && s.len() <= MAX_FIRMWARE_BYTES)
        else {
            return;
        };
        let state = info
            .get_property_val_str(txt::STATE)
            .filter(|s| s.len() <= MAX_STATE_BYTES)
            .unwrap_or("unknown");
        let upload_port = (info.get_property_val_str(txt::UPLOAD) == Some("grpc"))
            .then(|| {
                info.get_property_val_str(txt::GRPC_PORT)
                    .and_then(|s| s.parse::<u16>().ok())
                    .filter(|p| *p != 0)
            })
            .flatten();
        for ip in info.get_addresses_v4() {
            if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
                continue;
            }
            if self.devices.len() == MAX_DEVICES {
                self.limit_reached = true;
                break;
            }
            let address = SocketAddr::new(ip.into(), info.get_port());
            self.devices.insert(
                (name.to_owned(), address),
                Device {
                    address,
                    upload_address: upload_port.map(|port| SocketAddr::new(ip.into(), port)),
                    device_id: id.to_owned(),
                    firmware: firmware.to_owned(),
                    management_state: state.to_owned(),
                    ota_available: info.get_property_val_str(txt::OTA) == Some("1"),
                },
            );
        }
    }
    fn finish(self) -> Discovery {
        Discovery {
            devices: self.devices.into_values().collect(),
            limit_reached: self.limit_reached,
        }
    }
}
struct Daemon(ServiceDaemon);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.stop_browse(ota_protocol::SERVICE_TYPE);
        if let Ok(done) = self.0.shutdown() {
            let _ = done.recv_timeout(DAEMON_SHUTDOWN_TIMEOUT);
        }
    }
}
fn invalid(message: impl ToString) -> io::Error {
    io::Error::other(message.to_string())
}
/// Resolve IPv4 service endpoints during a collection window. TXT is advertised metadata,
/// not a live health assertion; use status for a direct query. mDNS owns its cache/packet limits.
pub fn discover(interface: Option<Ipv4Addr>, window: Duration) -> io::Result<Discovery> {
    if window < Duration::from_millis(MIN_SCAN_MS) || window > Duration::from_millis(MAX_SCAN_MS) {
        return Err(invalid(format!(
            "discovery window must be {MIN_SCAN_MS}..{MAX_SCAN_MS} ms"
        )));
    }
    let daemon = Daemon(ServiceDaemon::new().map_err(invalid)?);
    if let Some(ip) = interface {
        daemon.0.disable_interface(IfKind::All).map_err(invalid)?;
        daemon
            .0
            .enable_interface(IfKind::Addr(ip.into()))
            .map_err(invalid)?;
    }
    let events = daemon
        .0
        .browse(ota_protocol::SERVICE_TYPE)
        .map_err(invalid)?;
    let deadline = Instant::now() + window;
    let mut collector = Collector::default();
    while Instant::now() < deadline {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) => collector.event(event),
            Err(mdns_sd::RecvTimeoutError::Timeout) => break,
            Err(error) => return Err(invalid(error)),
        }
    }
    Ok(collector.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info(name: &str, port: u16, ip: &str) -> ResolvedService {
        mdns_sd::ServiceInfo::new(
            ota_protocol::SERVICE_TYPE,
            name,
            "device.local.",
            ip,
            port,
            [
                (txt::ID, "board"),
                (txt::FIRMWARE, "test"),
                (txt::PROTOCOL, "1"),
                (txt::STATE, "verified"),
                (txt::OTA, "1"),
                (txt::UPLOAD, "grpc"),
                (txt::GRPC_PORT, "34567"),
            ]
            .as_slice(),
        )
        .unwrap()
        .as_resolved_service()
    }
    #[test]
    fn uses_srv_port_and_txt_and_replaces_old_addresses() {
        let mut c = Collector::default();
        c.resolved(&info("test", 12345, "192.0.2.1"));
        c.resolved(&info("test", 23456, "192.0.2.2"));
        let r = c.finish();
        assert_eq!(r.devices.len(), 1);
        assert_eq!(r.devices[0].address, "192.0.2.2:23456".parse().unwrap());
        assert_eq!(r.devices[0].management_state, "verified");
        assert!(r.devices[0].ota_available);
        assert_eq!(
            r.devices[0].upload_address,
            Some("192.0.2.2:34567".parse().unwrap())
        );
    }
    #[test]
    fn removal_clears_instance_and_duplicates_do_not_multiply() {
        let mut c = Collector::default();
        let i = info("test", 123, "192.0.2.1");
        c.resolved(&i);
        c.resolved(&i);
        assert_eq!(c.devices.len(), 1);
        c.event(ServiceEvent::ServiceRemoved(
            ota_protocol::SERVICE_TYPE.into(),
            i.fullname.clone(),
        ));
        assert!(c.finish().devices.is_empty());
    }
    #[test]
    fn caps_results_and_rejects_incompatible_records() {
        let mut c = Collector::default();
        let bad = info("bad", 0, "192.0.2.1");
        c.resolved(&bad);
        assert!(c.devices.is_empty());
        for i in 0..70 {
            c.resolved(&info(&format!("device-{i}"), 123, "192.0.2.1"));
        }
        let r = c.finish();
        assert_eq!(r.devices.len(), 64);
        assert!(r.limit_reached);
    }
    #[test]
    fn incompatible_protocol_is_ignored_and_bad_windows_fail_before_network() {
        let bad = mdns_sd::ServiceInfo::new(
            ota_protocol::SERVICE_TYPE,
            "bad",
            "device.local.",
            "192.0.2.1",
            123,
            [
                (txt::ID, "board"),
                (txt::FIRMWARE, "test"),
                (txt::PROTOCOL, "2"),
            ]
            .as_slice(),
        )
        .unwrap()
        .as_resolved_service();
        let mut c = Collector::default();
        c.resolved(&bad);
        assert!(c.finish().devices.is_empty());
        assert!(discover(None, Duration::ZERO).is_err());
        assert!(discover(None, Duration::from_secs(61)).is_err());
    }
}
