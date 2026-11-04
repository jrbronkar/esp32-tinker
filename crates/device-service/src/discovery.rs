//! Standard mDNS/DNS-SD responder; edge-mdns owns DNS parsing and record encoding.
use core::net::{Ipv6Addr, SocketAddr};
use edge_mdns::{
    buf::VecBufAccess,
    domain::base::{
        iana::{Class, Rtype},
        Ttl,
    },
    host::{Host, Service, ServiceAnswers},
    io::{IP_BROADCAST_ADDR as MDNS_MULTICAST_ADDR, PORT as MDNS_PORT},
    HostAnswer, HostAnswers, MdnsError, MdnsHandler, MdnsRequest, MdnsResponse,
};
use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    Stack,
};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};
use embedded_io_async::{ErrorKind, ErrorType};
use ota_protocol::txt;

// Standard link-local mDNS IP hop limit (RFC 6762).
const MDNS_HOP_LIMIT: u8 = 255;
const DNS_CACHE_FLUSH_BIT: u16 = 0x8000;
const RECORD_TTL_SECS: u32 = 60;
const HOSTNAME_CAPACITY: usize = 32;
const DNS_MESSAGE_BYTES: usize = 1500;
const UDP_PACKET_SLOTS: usize = 2;
const UDP_BUFFER_BYTES: usize = UDP_PACKET_SLOTS * DNS_MESSAGE_BYTES;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const ADDRESS_POLL_INTERVAL: Duration = Duration::from_millis(500);

// Only socket I/O is adapted here. DNS and DNS-SD wire behavior come from edge-mdns.
#[derive(Clone, Copy)]
struct Socket<'a, 'd>(&'a UdpSocket<'d>);
impl ErrorType for Socket<'_, '_> {
    type Error = ErrorKind;
}
impl edge_nal::Readable for Socket<'_, '_> {
    async fn readable(&mut self) -> Result<(), ErrorKind> {
        self.0.wait_recv_ready().await;
        Ok(())
    }
}
impl edge_nal::UdpReceive for Socket<'_, '_> {
    async fn receive(&mut self, buf: &mut [u8]) -> Result<(usize, SocketAddr), ErrorKind> {
        // Oversized or truncated datagrams must not reach the DNS parser.
        loop {
            match self.0.recv_from(buf).await {
                Ok((len, peer)) => return Ok((len, peer.endpoint.into())),
                Err(embassy_net::udp::RecvError::Truncated) => continue,
            }
        }
    }
}
impl edge_nal::UdpSend for Socket<'_, '_> {
    async fn send(&mut self, peer: SocketAddr, buf: &[u8]) -> Result<(), ErrorKind> {
        let SocketAddr::V4(peer) = peer else {
            return Err(ErrorKind::Unsupported);
        };
        self.0
            .send_to(buf, embassy_net::IpEndpoint::from(peer))
            .await
            .map_err(|_| ErrorKind::Other)
    }
}
struct Random;
impl rand_core::TryRng for Random {
    type Error = core::convert::Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(esp_hal::rng::Rng::new().random())
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(((self.try_next_u32()? as u64) << u32::BITS) | self.try_next_u32()? as u64)
    }
    fn try_fill_bytes(&mut self, bytes: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in bytes.chunks_mut(core::mem::size_of::<u32>()) {
            chunk.copy_from_slice(&self.try_next_u32()?.to_le_bytes()[..chunk.len()]);
        }
        Ok(())
    }
}
struct Advertisement<'a> {
    host: Host<'a>,
    identity: &'a ota_protocol::StatusResponse,
    cache_flush: bool,
    runtime: &'a crate::runtime::Runtime,
}
impl HostAnswers for Advertisement<'_> {
    fn visit<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(HostAnswer) -> Result<(), E>,
        E: From<MdnsError>,
    {
        let txt = [
            (txt::ID, self.identity.device_id.as_str()),
            (txt::APPLICATION, self.identity.application.as_str()),
            (txt::FIRMWARE, self.identity.firmware_version.as_str()),
            (txt::PROTOCOL, ota_protocol::VERSION_TXT),
            (
                txt::UPLOAD,
                if cfg!(feature = "experimental-grpc") {
                    "grpc"
                } else {
                    "legacy"
                },
            ),
            (
                txt::GRPC_PORT,
                if cfg!(feature = "experimental-grpc") {
                    "7780"
                } else {
                    "0"
                },
            ),
            (txt::STATE, self.runtime.state()),
            (
                txt::OTA,
                if self.identity.ota_available {
                    "1"
                } else {
                    "0"
                },
            ),
        ];
        let service = Service {
            name: self.host.hostname,
            service: "_espctl",
            protocol: "_tcp",
            port: ota_protocol::PORT,
            priority: 0,
            weight: 0,
            service_subtypes: &[],
            txt_kvs: &txt,
        };
        ServiceAnswers::new(&self.host, &service).visit(|mut answer| {
            // Unique records replace cached addresses, endpoints and state. PTR is shared.
            if self.cache_flush && answer.rtype() != Rtype::PTR {
                answer.set_class(Class::from_int(Class::IN.to_int() | DNS_CACHE_FLUSH_BIT));
            }
            f(answer)
        })
    }
}
impl MdnsHandler for Advertisement<'_> {
    fn handle<'a>(
        &mut self,
        request: MdnsRequest<'_>,
        buffer: &'a mut [u8],
    ) -> Result<MdnsResponse<'a>, MdnsError> {
        // Legacy DNS replies must use ordinary IN records (RFC 6762 section 6.7).
        self.cache_flush = !matches!(request, MdnsRequest::Request { legacy: true, .. });
        crate::mdns_filter::respond(&*self, request, buffer)
    }
}
pub async fn serve(
    stack: Stack<'static>,
    identity: &ota_protocol::StatusResponse,
    runtime: &crate::runtime::Runtime,
) {
    let mut name = heapless::String::<HOSTNAME_CAPACITY>::new();
    for ch in identity
        .device_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
    {
        if name.push(ch).is_err() {
            esp_println::println!("mDNS hostname exceeds bound");
            return;
        }
    }
    let mut rx_meta = [PacketMetadata::EMPTY; UDP_PACKET_SLOTS];
    let mut tx_meta = [PacketMetadata::EMPTY; UDP_PACKET_SLOTS];
    let mut rx = [0; UDP_BUFFER_BYTES];
    let mut tx = [0; UDP_BUFFER_BYTES];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.set_hop_limit(Some(MDNS_HOP_LIMIT));
    while socket.bind(MDNS_PORT).is_err() {
        Timer::after(RETRY_DELAY).await;
    }
    loop {
        stack.wait_config_up().await;
        let Some(config) = stack.config_v4() else {
            continue;
        };
        let ip = config.address.address();
        if stack
            .join_multicast_group(embassy_net::IpAddress::Ipv4(MDNS_MULTICAST_ADDR))
            .is_err()
        {
            Timer::after(RETRY_DELAY).await;
            continue;
        }
        let recv = VecBufAccess::<NoopRawMutex, DNS_MESSAGE_BYTES>::new();
        let send = VecBufAccess::<NoopRawMutex, DNS_MESSAGE_BYTES>::new();
        let mdns = edge_mdns::io::Mdns::new(
            Some(ip),
            None,
            Socket(&socket),
            Socket(&socket),
            &recv,
            &send,
            Random,
            &runtime.announce,
        );
        let advertisement = Advertisement {
            host: Host {
                hostname: &name,
                ipv4: ip,
                ipv6: Ipv6Addr::UNSPECIFIED,
                ttl: Ttl::from_secs(RECORD_TTL_SECS),
            },
            identity,
            cache_flush: true,
            runtime,
        };
        let change = async {
            loop {
                Timer::after(ADDRESS_POLL_INTERVAL).await;
                if stack.config_v4().map(|c| c.address.address()) != Some(ip) {
                    break;
                }
            }
        };
        // Rebuild records if DHCP changes; never keep advertising a captured old address.
        embassy_futures::select::select(mdns.run(advertisement), change).await;
        let _ = stack.leave_multicast_group(embassy_net::IpAddress::Ipv4(MDNS_MULTICAST_ADDR));
        Timer::after(RETRY_DELAY).await;
    }
}
