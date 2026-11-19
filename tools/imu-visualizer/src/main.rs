use std::{
    env,
    io::{BufRead, BufReader},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use imu_telemetry::OrientationSample;
use imu_visualizer::parse_sample;
use macroquad::prelude::*;
use rumqttc::v5::{
    mqttbytes::{
        v5::{Packet, SubscribeReasonCode},
        QoS,
    },
    Client, Event, MqttOptions,
};

#[macroquad::main("ESP32-S3 IMU Orientation")]
async fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let (receiver, source_name) = match arguments.as_slice() {
        [flag, host, device, rest @ ..] if flag == "--mqtt" => {
            let port = match rest {
                [] => Some(1883),
                [port] => port.parse::<u16>().ok().filter(|port| *port != 0),
                _ => None,
            };
            let Some(port) = port else {
                eprintln!("MQTT port must be a number from 1 to 65535");
                return;
            };
            if !(1..=48).contains(&device.len())
                || !device
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                eprintln!("MQTT device ID must be 1–48 letters, digits, hyphens or underscores");
                return;
            }
            let topic = format!("devices/{device}/imu");
            let source = format!("MQTT {host}:{port} {topic}");
            (mqtt_reader(host.clone(), port, topic), source)
        }
        [port_name] => (serial_reader(port_name.clone()), port_name.clone()),
        _ => {
            eprintln!("usage: imu-visualizer /dev/cu.usbmodemXXXX | --mqtt HOST DEVICE_ID [PORT]");
            return;
        }
    };
    let mut latest = None;
    let mut status = format!("Waiting for orientation on {source_name}");

    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                Ok(sample) => {
                    latest = Some(sample);
                    status = format!("Live on {source_name}");
                }
                Err(error) => status = error,
            }
        }

        clear_background(Color::from_rgba(18, 22, 30, 255));
        draw_scene(latest.as_ref().map_or(Quat::IDENTITY, rotation));
        draw_overlay(&status, latest.as_ref());
        next_frame().await;
    }
}

fn mqtt_reader(
    host: String,
    port: u16,
    topic: String,
) -> Receiver<Result<OrientationSample, String>> {
    let (sender, receiver) = mpsc::sync_channel(32);

    thread::spawn(move || {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let client_id = format!("imu-viewer-{}-{}", std::process::id(), started.as_nanos());
        let options = MqttOptions::new(client_id, host, port);
        let (client, mut connection) = Client::new(options, 4);
        for event in connection.iter() {
            match event {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    if let Err(error) = client.try_subscribe(&topic, QoS::AtMostOnce) {
                        let _ = sender.try_send(Err(format!("MQTT subscribe failed: {error}")));
                    }
                }
                Ok(Event::Incoming(Packet::SubAck(ack)))
                    if !matches!(
                        ack.return_codes.as_slice(),
                        [SubscribeReasonCode::Success(_)]
                    ) =>
                {
                    let _ = sender.try_send(Err(format!(
                        "MQTT broker rejected subscription: {:?}",
                        ack.return_codes
                    )));
                }
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    let sample = imu_telemetry::decode(&publish.payload)
                        .map_err(|error| format!("Invalid MQTT orientation payload: {error:?}"));
                    let _ = sender.try_send(sample);
                }
                Err(error) => {
                    let _ = sender.try_send(Err(format!("MQTT connection failed: {error}")));
                    thread::sleep(Duration::from_secs(1));
                }
                _ => {}
            }
        }
    });

    receiver
}

fn serial_reader(port_name: String) -> Receiver<Result<OrientationSample, String>> {
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        let port = match serialport::new(&port_name, 115_200)
            .timeout(Duration::from_millis(100))
            .open()
        {
            Ok(port) => port,
            Err(error) => {
                let _ = sender.send(Err(format!("Could not open {port_name}: {error}")));
                return;
            }
        };

        let mut reader = BufReader::new(port);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => continue,
                Ok(_) => {
                    if let Some(sample) = parse_sample(&line) {
                        let _ = sender.send(Ok(sample));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(error) => {
                    let _ = sender.send(Err(format!("Serial read failed: {error}")));
                    return;
                }
            }
        }
    });

    receiver
}

// The sensors use Z as up. The scene uses Y as up and -Z for sensor Y.
fn sensor_to_scene() -> Quat {
    Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)
}

fn rotation(sample: &OrientationSample) -> Quat {
    let basis = sensor_to_scene();
    basis * Quat::from_array(sample.xyzw()) * basis.conjugate()
}

fn draw_scene(rotation: Quat) {
    set_camera(&Camera3D {
        position: vec3(4.5, 3.2, 5.5),
        target: Vec3::ZERO,
        up: Vec3::Y,
        ..Default::default()
    });

    draw_grid(20, 0.25, GRAY, DARKGRAY);
    draw_rotated_box(rotation);
    set_default_camera();
}

fn draw_rotated_box(rotation: Quat) {
    let half = vec3(1.5, 0.12, 0.9);
    let vertices = [
        vec3(-half.x, -half.y, -half.z),
        vec3(half.x, -half.y, -half.z),
        vec3(half.x, half.y, -half.z),
        vec3(-half.x, half.y, -half.z),
        vec3(-half.x, -half.y, half.z),
        vec3(half.x, -half.y, half.z),
        vec3(half.x, half.y, half.z),
        vec3(-half.x, half.y, half.z),
    ]
    .map(|point| rotation * point);

    for (a, b) in [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ] {
        draw_line_3d(vertices[a], vertices[b], SKYBLUE);
    }

    let origin = Vec3::ZERO;
    let axes = rotation * sensor_to_scene();
    draw_line_3d(origin, axes * vec3(2.2, 0.0, 0.0), RED);
    draw_line_3d(origin, axes * vec3(0.0, 2.2, 0.0), GREEN);
    draw_line_3d(origin, axes * vec3(0.0, 0.0, 2.2), BLUE);
}

fn draw_overlay(status: &str, sample: Option<&OrientationSample>) {
    draw_text("ESP32-S3 IMU", 24.0, 36.0, 30.0, WHITE);
    draw_text(status, 24.0, 64.0, 20.0, LIGHTGRAY);
    if let Some(sample) = sample {
        let [x, y, z, w] = sample.xyzw();
        draw_text(
            &format!(
                "xyzw [{x:+.4}, {y:+.4}, {z:+.4}, {w:+.4}]    {} ms",
                sample.sample_ms
            ),
            24.0,
            94.0,
            20.0,
            LIGHTGRAY,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_turns_the_board_in_the_horizontal_plane() {
        let turn = rotation(&OrientationSample {
            z: std::f32::consts::FRAC_1_SQRT_2,
            w: std::f32::consts::FRAC_1_SQRT_2,
            ..Default::default()
        });
        assert!((turn * Vec3::Y - Vec3::Y).length() < 1e-5);
        assert!((turn * Vec3::X + Vec3::Z).length() < 1e-5);
    }
}
