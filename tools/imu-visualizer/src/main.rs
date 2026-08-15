use std::{
    env,
    io::{BufRead, BufReader},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

use macroquad::prelude::*;

#[derive(Clone, Copy, Debug)]
struct Sample {
    time_ms: u64,
    accel: Vec3,
    gyro_dps: Vec3,
    temperature_c: f32,
}

#[derive(Clone, Copy, Debug, Default)]
struct Orientation {
    roll: f32,
    pitch: f32,
    yaw: f32,
    last_time_ms: Option<u64>,
}

impl Orientation {
    fn update(&mut self, sample: Sample) {
        let accel_roll = sample.accel.y.atan2(sample.accel.z);
        let accel_pitch = (-sample.accel.x)
            .atan2((sample.accel.y * sample.accel.y + sample.accel.z * sample.accel.z).sqrt());

        let Some(previous_ms) = self.last_time_ms.replace(sample.time_ms) else {
            self.roll = accel_roll;
            self.pitch = accel_pitch;
            return;
        };

        let dt = (sample.time_ms.saturating_sub(previous_ms) as f32 / 1_000.0).min(0.1);
        let gyro = vec3(
            sample.gyro_dps.x.to_radians(),
            sample.gyro_dps.y.to_radians(),
            sample.gyro_dps.z.to_radians(),
        );

        self.roll += gyro.x * dt;
        self.pitch += gyro.y * dt;
        self.yaw += gyro.z * dt;

        // Gyroscope integration is responsive; gravity corrects long-term roll/pitch drift.
        const GYRO_WEIGHT: f32 = 0.98;
        self.roll = GYRO_WEIGHT * self.roll + (1.0 - GYRO_WEIGHT) * accel_roll;
        self.pitch = GYRO_WEIGHT * self.pitch + (1.0 - GYRO_WEIGHT) * accel_pitch;
    }

    fn rotation(self) -> Quat {
        Quat::from_euler(EulerRot::ZYX, self.yaw, self.pitch, self.roll)
    }
}

#[macroquad::main("ESP32-S3 IMU Orientation")]
async fn main() {
    let Some(port_name) = env::args().nth(1) else {
        eprintln!("usage: imu-visualizer /dev/cu.usbmodemXXXX");
        return;
    };

    let receiver = serial_reader(port_name.clone());
    let mut orientation = Orientation::default();
    let mut latest = None;
    let mut status = format!("Waiting for IMU data on {port_name}");

    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                Ok(sample) => {
                    orientation.update(sample);
                    latest = Some(sample);
                    status = "Live".to_owned();
                }
                Err(error) => status = error,
            }
        }

        clear_background(Color::from_rgba(18, 22, 30, 255));
        draw_scene(orientation.rotation());
        draw_overlay(&status, latest, orientation);
        next_frame().await;
    }
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

fn parse_sample(line: &str) -> Option<Sample> {
    let mut fields = line.split(',');
    if fields.next()? != "IMU" {
        return None;
    }

    Some(Sample {
        time_ms: fields.next()?.parse().ok()?,
        accel: vec3(
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
        ),
        gyro_dps: vec3(
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
            fields.next()?.parse().ok()?,
        ),
        temperature_c: fields.next()?.parse().ok()?,
    })
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
                sample.accel.x, sample.accel.y, sample.accel.z
            ),
            24.0,
            124.0,
            20.0,
            LIGHTGRAY,
        );
        draw_text(
            &format!(
                "gyro  [{:+.1}, {:+.1}, {:+.1}] °/s    {:.1} °C",
                sample.gyro_dps.x, sample.gyro_dps.y, sample.gyro_dps.z, sample.temperature_c
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_telemetry_and_ignores_diagnostics() {
        assert!(parse_sample("MPU6050 confirmed and awake").is_none());
        let sample = parse_sample("IMU,120,0.1,-0.2,0.98,1.0,-2.0,3.0,27.5").unwrap();
        assert_eq!(sample.time_ms, 120);
        assert_eq!(sample.accel, vec3(0.1, -0.2, 0.98));
        assert_eq!(sample.gyro_dps, vec3(1.0, -2.0, 3.0));
        assert_eq!(sample.temperature_c, 27.5);
    }
}
