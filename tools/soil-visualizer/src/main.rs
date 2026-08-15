use std::{
    collections::VecDeque,
    env,
    net::UdpSocket,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use macroquad::prelude::*;
use soil_visualizer::{parse_sample, Sample};

const HISTORY_LENGTH: usize = 180;

#[macroquad::main("ESP32 Soil Moisture")]
async fn main() {
    let port = match env::args().nth(1) {
        Some(value) => match value.parse::<u16>().ok().filter(|port| *port != 0) {
            Some(port) => port,
            None => {
                eprintln!("usage: soil-visualizer [udp-port]");
                return;
            }
        },
        None => 7777,
    };
    let receiver = udp_reader(port);
    let mut latest = None;
    let mut last_received = None;
    let mut history = VecDeque::with_capacity(HISTORY_LENGTH);
    let mut error = None;

    loop {
        while let Ok(message) = receiver.try_recv() {
            match message {
                Ok(sample) => {
                    if history.len() == HISTORY_LENGTH {
                        history.pop_front();
                    }
                    history.push_back(sample.moisture_percent);
                    latest = Some(sample);
                    last_received = Some(Instant::now());
                    error = None;
                }
                Err(message) => error = Some(message),
            }
        }

        clear_background(Color::from_rgba(16, 21, 28, 255));
        draw_dashboard(port, latest, last_received, &history, error.as_deref());
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
                Ok((length, _)) => {
                    let Ok(text) = std::str::from_utf8(&packet[..length]) else {
                        continue;
                    };
                    for line in text.lines() {
                        if let Some(sample) = parse_sample(line) {
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

fn draw_dashboard(
    port: u16,
    sample: Option<Sample>,
    last_received: Option<Instant>,
    history: &VecDeque<u8>,
    error: Option<&str>,
) {
    let margin = 32.0;
    draw_text("ESP32 Soil Monitor", margin, 48.0, 34.0, WHITE);
    let live = last_received
        .map(|instant| instant.elapsed() < Duration::from_secs(3))
        .unwrap_or(false);
    let status = error.map(str::to_owned).unwrap_or_else(|| {
        if live {
            "LIVE".to_owned()
        } else {
            format!("Waiting on UDP {port}")
        }
    });
    draw_text(&status, margin, 76.0, 20.0, if live { GREEN } else { GRAY });

    let percent = sample.map(|value| value.moisture_percent).unwrap_or(0);
    let color = moisture_color(percent);
    let center = vec2(170.0, 220.0);
    draw_circle_lines(
        center.x,
        center.y,
        105.0,
        18.0,
        Color::from_rgba(48, 57, 68, 255),
    );
    draw_circle(center.x, center.y, 86.0, Color::from_rgba(25, 31, 40, 255));
    draw_text_ex(
        &format!("{percent}%"),
        center.x - 62.0,
        center.y + 18.0,
        TextParams {
            font_size: 52,
            color,
            ..Default::default()
        },
    );
    draw_text(
        watering_label(percent),
        center.x - 62.0,
        center.y + 54.0,
        20.0,
        color,
    );

    let card_x = 320.0;
    draw_rectangle(
        card_x,
        115.0,
        screen_width() - card_x - margin,
        210.0,
        Color::from_rgba(25, 31, 40, 255),
    );
    draw_text("Sensor readings", card_x + 24.0, 150.0, 25.0, WHITE);
    if let Some(sample) = sample {
        draw_text(
            &format!("Raw ADC       {}", sample.raw_adc),
            card_x + 24.0,
            195.0,
            23.0,
            LIGHTGRAY,
        );
        draw_text(
            &format!("Filtered ADC  {}", sample.filtered_adc),
            card_x + 24.0,
            232.0,
            23.0,
            LIGHTGRAY,
        );
        draw_text(
            &format!("Device time   {:.1} s", sample.time_ms as f64 / 1000.0),
            card_x + 24.0,
            269.0,
            23.0,
            LIGHTGRAY,
        );
    }
    draw_history(
        history,
        margin,
        360.0,
        screen_width() - margin * 2.0,
        screen_height() - 400.0,
    );
}

fn draw_history(history: &VecDeque<u8>, x: f32, y: f32, width: f32, height: f32) {
    draw_text("Moisture history", x, y - 14.0, 24.0, WHITE);
    draw_rectangle(
        x,
        y,
        width,
        height.max(100.0),
        Color::from_rgba(25, 31, 40, 255),
    );
    if history.len() < 2 {
        return;
    }
    let height = height.max(100.0);
    for index in 1..history.len() {
        let previous = history[index - 1];
        let current = history[index];
        let x1 = x + width * (index - 1) as f32 / (HISTORY_LENGTH - 1) as f32;
        let x2 = x + width * index as f32 / (HISTORY_LENGTH - 1) as f32;
        let y1 = y + height * (1.0 - f32::from(previous) / 100.0);
        let y2 = y + height * (1.0 - f32::from(current) / 100.0);
        draw_line(x1, y1, x2, y2, 3.0, moisture_color(current));
    }
}

fn moisture_color(percent: u8) -> Color {
    Color::new(
        1.0 - f32::from(percent) / 100.0,
        f32::from(percent) / 100.0,
        0.08,
        1.0,
    )
}

fn watering_label(percent: u8) -> &'static str {
    match percent {
        0..=25 => "WATER NOW",
        26..=55 => "GETTING DRY",
        _ => "MOIST",
    }
}
