use embassy_net::{
    udp::{PacketMetadata, UdpSocket},
    Stack,
};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use ota_protocol::{Request, StatusResponse, DISCOVERY_PORT, MAX_FRAME};
#[embassy_executor::task]
pub async fn server(stack: Stack<'static>, mut identity: StatusResponse) {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx = [0; MAX_FRAME * 2];
    let mut tx = [0; MAX_FRAME * 2];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    while socket.bind(DISCOVERY_PORT).is_err() {
        Timer::after(Duration::from_secs(1)).await;
    }
    identity.clear_boot();
    identity.clear_timings();
    let mut packet = [0; MAX_FRAME];
    loop {
        stack.wait_config_up().await;
        let Ok((n, peer)) = socket.recv_from(&mut packet).await else {
            continue;
        };
        let Ok(request) = ota_protocol::decode::<Request>(&packet[..n]) else {
            continue;
        };
        identity.uptime_ms = Instant::now().as_millis();
        identity.management_state = crate::runtime::state().try_into().unwrap_or_default();
        let Some(response) = ota_protocol::discovery_response(request, identity.clone()) else {
            continue;
        };
        if let Ok(body) = ota_protocol::encode(&response) {
            // Received metadata carries the broadcast destination as local_address.
            // Reply only to the remote endpoint so the stack selects our unicast IP.
            let _ =
                with_timeout(Duration::from_secs(1), socket.send_to(&body, peer.endpoint)).await;
        }
        // Cap response rate; discovery is informational and unauthenticated.
        Timer::after(Duration::from_millis(200)).await;
    }
}
