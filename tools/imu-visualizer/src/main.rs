use std::{
    env,
    io::{BufRead, BufReader},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use imu_visualizer::{
    parse_magnetic, parse_sample, MagneticSample, Orientation, OrientationEstimator, Sample,
};
use macroquad::prelude::*;
use rumqttc::v5::{
    mqttbytes::{
        v5::{Packet, SubscribeReasonCode},
        QoS,
    },
    Client, Event, MqttOptions,
};

mod mqtt_payload;

enum Telemetry {
    Imu(Sample),
    Magnetic(MagneticSample),
}

fn parse_telemetry(line: &str) -> Option<Telemetry> {
    parse_sample(line)
        .map(Telemetry::Imu)
        .or_else(|| parse_magnetic(line).map(Telemetry::Magnetic))
}

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
    let mut estimator = OrientationEstimator::default();
    let mut latest: Option<Sample> = None;
    let mut magnetic: Option<(MagneticSample, Instant)> = None;
    let mut status = format!("Waiting for IMU data on {source_name}");

    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                Ok(Telemetry::Imu(sample)) => {
                    if latest.is_some_and(|previous| sample.time_ms < previous.time_ms) {
                        estimator = OrientationEstimator::default();
                        magnetic = None;
                    }
                    estimator.update(sample);
                    latest = Some(sample);
                    status = "Live".to_owned();
                }
                Ok(Telemetry::Magnetic(sample)) => {
                    if magnetic.is_none_or(|(previous, _)| sample.time_ms > previous.time_ms)
                        && estimator.correct_yaw(sample)
                    {
                        magnetic = Some((sample, Instant::now()));
                    }
                }
                Err(error) => status = error,
            }
        }

        clear_background(Color::from_rgba(18, 22, 30, 255));
        let orientation = estimator.orientation();
        draw_scene(rotation(orientation));
        let fresh_field = magnetic
            .filter(|(_, received)| received.elapsed() < Duration::from_secs(1))
            .map(|(sample, _)| sample.field_ut);
        draw_overlay(&status, latest, orientation, fresh_field);
        next_frame().await;
    }
}

fn mqtt_reader(host: String, port: u16, topic: String) -> Receiver<Result<Telemetry, String>> {
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
                    let Ok((motion, magnetic)) = mqtt_payload::decode(&publish.payload) else {
                        let _ = sender.try_send(Err("Invalid MQTT IMU payload".to_owned()));
                        continue;
                    };
                    // Update the gyro estimate before applying a magnetic correction from
                    // the same publication.
                    if let Some(sample) = motion {
                        let _ = sender.try_send(Ok(Telemetry::Imu(sample)));
                    }
                    if let Some(sample) = magnetic {
                        let _ = sender.try_send(Ok(Telemetry::Magnetic(sample)));
                    }
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

fn serial_reader(port_name: String) -> Receiver<Result<Telemetry, String>> {
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
                    if let Some(sample) = parse_telemetry(line.trim()) {
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

fn rotation(orientation: Orientation) -> Quat {
    let basis = sensor_to_scene();
    let sensor_rotation = Quat::from_euler(
        EulerRot::ZYX,
        orientation.yaw,
        orientation.pitch,
        orientation.roll,
    );
    basis * sensor_rotation * basis.conjugate()
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

fn draw_overlay(
    status: &str,
    sample: Option<Sample>,
    orientation: Orientation,
    field: Option<[f32; 3]>,
) {
    draw_text("ESP32-S3 IMU", 24.0, 36.0, 30.0, WHITE);
    draw_text(status, 24.0, 64.0, 20.0, LIGHTGRAY);

    let degrees = vec3(
        orientation.roll.to_degrees(),
        orientation.pitch.to_degrees(),
        orientation.yaw.to_degrees(),
    );
    draw_text(
        &format!(
            "roll {:7.1}°   pitch {:7.1}°   yaw {:7.1}°",
            degrees.x, degrees.y, degrees.z
        ),
        24.0,
        94.0,
        24.0,
        WHITE,
    );

    if let Some(sample) = sample {
        draw_text(
            &format!(
                "accel [{:+.3}, {:+.3}, {:+.3}] g",
                sample.accel_g[0], sample.accel_g[1], sample.accel_g[2]
            ),
            24.0,
            124.0,
            20.0,
            LIGHTGRAY,
        );
        draw_text(
            &format!(
                "gyro  [{:+.1}, {:+.1}, {:+.1}] °/s    {:.1} °C",
                sample.gyro_dps[0], sample.gyro_dps[1], sample.gyro_dps[2], sample.temperature_c
            ),
            24.0,
            150.0,
            20.0,
            LIGHTGRAY,
        );
    }

    let yaw_status = match field {
        Some([x, y, z]) => format!("Yaw corrected by BMM150    [{x:+.1}, {y:+.1}, {z:+.1}] µT"),
        None => "Yaw from gyroscope    Waiting for BMM150 data".to_owned(),
    };
    draw_text(&yaw_status, 24.0, screen_height() - 24.0, 18.0, GRAY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_turns_the_board_in_the_horizontal_plane() {
        let turn = rotation(Orientation {
            yaw: std::f32::consts::FRAC_PI_2,
            ..Orientation::default()
        });
        assert!((turn * Vec3::Y - Vec3::Y).length() < 1e-5);
        assert!((turn * Vec3::X + Vec3::Z).length() < 1e-5);
    }
}
