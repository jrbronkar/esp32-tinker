use std::{
    env,
    io::{BufRead, BufReader},
    net::UdpSocket,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use imu_visualizer::{parse_sample, Orientation, OrientationEstimator, Sample};
use macroquad::prelude::*;

#[macroquad::main("ESP32-S3 IMU Orientation")]
async fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let (receiver, source_name) = match arguments.as_slice() {
        [flag] if flag == "--udp" => (udp_reader(7778), "UDP port 7778".to_owned()),
        [flag, port] if flag == "--udp" => match port.parse::<u16>().ok().filter(|port| *port != 0)
        {
            Some(port) => (udp_reader(port), format!("UDP port {port}")),
            None => {
                eprintln!("UDP port must be a number from 1 to 65535");
                return;
            }
        },
        [port_name] => (serial_reader(port_name.clone()), port_name.clone()),
        _ => {
            eprintln!("usage: imu-visualizer /dev/cu.usbmodemXXXX | --udp [port]");
            return;
        }
    };
    let mut estimator = OrientationEstimator::default();
    let mut latest = None;
    let mut status = format!("Waiting for IMU data on {source_name}");

    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                Ok(sample) => {
                    estimator.update(sample);
                    latest = Some(sample);
                    status = "Live".to_owned();
                }
                Err(error) => status = error,
            }
        }

        clear_background(Color::from_rgba(18, 22, 30, 255));
        let orientation = estimator.orientation();
        draw_scene(rotation(orientation));
        draw_overlay(&status, latest, orientation);
        next_frame().await;
    }
}

fn udp_reader(port: u16) -> Receiver<Result<Sample, String>> {
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        let socket = match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(socket) => socket,
            Err(error) => {
                let _ = sender.send(Err(format!("Could not listen on UDP port {port}: {error}")));
                return;
            }
        };
        let mut packet = [0_u8; 512];
        loop {
            match socket.recv_from(&mut packet) {
                Ok((length, _source)) => {
                    let Ok(text) = std::str::from_utf8(&packet[..length]) else {
                        continue;
                    };
                    for line in text.lines() {
                        if let Some(sample) = parse_sample(line.trim()) {
                            let _ = sender.send(Ok(sample));
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(format!("UDP receive failed: {error}")));
                    return;
                }
            }
        }
    });

    receiver
}

fn serial_reader(port_name: String) -> Receiver<Result<Sample, String>> {
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
                    if let Some(sample) = parse_sample(line.trim()) {
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

fn rotation(orientation: Orientation) -> Quat {
    Quat::from_euler(
        EulerRot::ZYX,
        orientation.yaw,
        orientation.pitch,
        orientation.roll,
    )
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
    draw_line_3d(origin, rotation * vec3(2.2, 0.0, 0.0), RED);
    draw_line_3d(origin, rotation * vec3(0.0, 2.2, 0.0), GREEN);
    draw_line_3d(origin, rotation * vec3(0.0, 0.0, 2.2), BLUE);
}

fn draw_overlay(status: &str, sample: Option<Sample>, orientation: Orientation) {
    draw_text("ESP32-S3 MPU6050", 24.0, 36.0, 30.0, WHITE);
    draw_text(status, 24.0, 64.0, 20.0, LIGHTGRAY);

    let degrees = vec3(
        orientation.roll.to_degrees(),
        orientation.pitch.to_degrees(),
        orientation.yaw.to_degrees(),
    );
    draw_text(
        &format!(
            "roll {:7.1}°   pitch {:7.1}°   yaw* {:7.1}°",
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

    draw_text(
        "* yaw is gyro-only and will drift without a magnetometer",
        24.0,
        screen_height() - 24.0,
        18.0,
        GRAY,
    );
}
