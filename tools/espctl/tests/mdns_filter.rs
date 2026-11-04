// Compile the hardware-independent responder wrapper on the host; the full
// device-service crate depends on the ESP32 HAL.
#[path = "../../../crates/device-service/src/mdns_filter.rs"]
mod mdns_filter;

use edge_mdns::{
    domain::base::{iana::Rtype, Message, Ttl},
    host::{Host, Service, ServiceAnswers},
    MdnsRequest, MdnsResponse,
};

fn query(questions: &[(&str, u16, u16)]) -> Vec<u8> {
    let mut packet = vec![0; 12];
    packet[4..6].copy_from_slice(&(questions.len() as u16).to_be_bytes());
    for (name, kind, class) in questions {
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(&class.to_be_bytes());
    }
    packet
}
fn response(packet: &[u8]) -> Option<Vec<u8>> {
    let host = Host {
        hostname: "board",
        ipv4: "192.0.2.1".parse().unwrap(),
        ipv6: core::net::Ipv6Addr::UNSPECIFIED,
        ttl: Ttl::from_secs(60),
    };
    let service = Service {
        name: "board",
        service: "_espctl",
        protocol: "_tcp",
        port: 12345,
        priority: 0,
        weight: 0,
        service_subtypes: &[],
        txt_kvs: &[("firmware", "test")],
    };
    let mut buffer = [0; 1500];
    match mdns_filter::respond(
        &ServiceAnswers::new(&host, &service),
        MdnsRequest::Request {
            data: packet,
            legacy: false,
            multicast: true,
        },
        &mut buffer,
    )
    .unwrap()
    {
        MdnsResponse::None => None,
        MdnsResponse::Reply { data, .. } => Some(data.to_vec()),
    }
}
fn answer_types(packet: &[u8]) -> Vec<Rtype> {
    Message::from_octets(packet)
        .unwrap()
        .answer()
        .unwrap()
        .map(|r| r.unwrap().rtype())
        .collect()
}
#[test]
fn unrelated_names_and_incoming_responses_are_ignored() {
    assert!(response(&query(&[("_http._tcp.local", 12, 1)])).is_none());
    let mut packet = query(&[("board.local", 1, 1)]);
    packet[2] = 0x80;
    assert!(response(&packet).is_none());
}
#[test]
fn matching_name_requires_requested_record_type() {
    assert!(response(&query(&[("board.local", 28, 1)])).is_none());
    let packet = response(&query(&[("board._espctl._tcp.local", 16, 1)])).unwrap();
    assert_eq!(answer_types(&packet), [Rtype::TXT]);
}
#[test]
fn matching_name_requires_class_and_accepts_unicast_response_bit() {
    assert!(response(&query(&[("board.local", 1, 3)])).is_none());
    let packet = response(&query(&[("board.local", 1, 0x8001)])).unwrap();
    assert_eq!(answer_types(&packet), [Rtype::A]);
}
#[test]
fn ptr_resolution_keeps_address_service_and_metadata_additionals() {
    let packet = response(&query(&[("_espctl._tcp.local", 12, 1)])).unwrap();
    assert_eq!(answer_types(&packet), [Rtype::PTR]);
    let message = Message::from_octets(packet.as_slice()).unwrap();
    let additional: Vec<_> = message
        .additional()
        .unwrap()
        .map(|r| r.unwrap().rtype())
        .collect();
    assert!(additional.contains(&Rtype::A));
    assert!(additional.contains(&Rtype::SRV));
    assert!(additional.contains(&Rtype::TXT));
}
#[test]
fn any_queries_and_multiple_questions_are_supported() {
    let packet = response(&query(&[("board.local", 255, 255)])).unwrap();
    assert_eq!(answer_types(&packet), [Rtype::A]);
    let packet = response(&query(&[
        ("board.local", 1, 1),
        ("board._espctl._tcp.local", 16, 1),
    ]))
    .unwrap();
    assert_eq!(answer_types(&packet), [Rtype::A, Rtype::TXT]);
}
