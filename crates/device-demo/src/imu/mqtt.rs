use core::{cell::Cell, net::SocketAddrV4};

use device_service::{DeviceLogs, LogSeverity};
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{with_timeout, Duration, Ticker, Timer};
use heapless::String;
use imu_telemetry::{ImuTelemetry, MagneticSample, MotionSample};
use rust_mqtt::{
    buffer::BumpBuffer,
    client::{
        options::{ConnectOptions, PublicationOptions, TopicReference},
        Client,
    },
    types::{MqttString, TopicName},
};

// Each sensor replaces its pending sample, so a slow connection cannot build a backlog.
#[derive(Default)]
pub(super) struct Samples {
    pub motion: Cell<Option<MotionSample>>,
    pub magnetic: Cell<Option<MagneticSample>>,
}

pub(super) async fn run(stack: Stack<'static>, logs: &DeviceLogs, samples: &Samples) {
    let endpoint =
        option_env!("IMU_MQTT_BROKER").and_then(|value| value.parse::<SocketAddrV4>().ok());
    let device = option_env!("IMU_MQTT_DEVICE_ID").filter(|value| {
        (1..=48).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    let (Some(endpoint), Some(device)) = (endpoint, device) else {
        logs.log(
            LogSeverity::Warn,
            "Set IMU_MQTT_BROKER and IMU_MQTT_DEVICE_ID",
        );
        return;
    };
    let mut topic = String::<64>::new();
    if topic.push_str("devices/").is_err()
        || topic.push_str(device).is_err()
        || topic.push_str("/imu").is_err()
    {
        return;
    }
    let Ok(topic_string) = MqttString::try_from(topic.as_str()) else {
        return;
    };
    let Some(topic) = TopicName::new(topic_string) else {
        return;
    };
    let Ok(client_id) = MqttString::try_from(device) else {
        return;
    };
    let mut rx = [0; 512];
    let mut tx = [0; 512];
    let mut mqtt_buffer = [0; 512];
    loop {
        stack.wait_config_up().await;
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_keep_alive(Some(Duration::from_secs(5)));
        socket.set_timeout(Some(Duration::from_secs(15)));

        let connected = matches!(
            with_timeout(Duration::from_secs(5), socket.connect(endpoint)).await,
            Ok(Ok(()))
        );
        if connected {
            let mut buffer = BumpBuffer::new(&mut mqtt_buffer);
            let mut client = Client::<'_, '_, _, _, 1, 1, 1, 1, 1, 0, 1>::new(&mut buffer);
            let options = ConnectOptions::new().clean_start();
            if matches!(
                with_timeout(
                    Duration::from_secs(5),
                    client.connect(socket, &options, Some(client_id.as_borrowed()))
                )
                .await,
                Ok(Ok(_))
            ) {
                logs.log(LogSeverity::Info, "IMU MQTT connected");
                samples.motion.set(None);
                samples.magnetic.set(None);
                let publication =
                    PublicationOptions::new(TopicReference::Name(topic.as_borrowed()));
                let mut ticker = Ticker::every(Duration::from_millis(20));
                loop {
                    ticker.next().await;
                    let motion = samples.motion.take();
                    let magnetic = samples.magnetic.take();
                    if motion.is_none() && magnetic.is_none() {
                        continue;
                    }
                    let mut sample = ImuTelemetry::default();
                    if let Some(motion) = motion {
                        sample.set_motion(motion);
                    }
                    if let Some(magnetic) = magnetic {
                        sample.set_magnetic(magnetic);
                    }
                    let Ok(payload) = imu_telemetry::encode(&sample) else {
                        logs.log(LogSeverity::Error, "IMU MQTT payload exceeded buffer");
                        continue;
                    };
                    if !matches!(
                        with_timeout(
                            Duration::from_millis(250),
                            client.publish(&publication, payload.as_slice().into())
                        )
                        .await,
                        Ok(Ok(_))
                    ) {
                        break;
                    }
                    // Skip elapsed ticks after a blocked send and resume with the latest sample.
                    ticker.reset();
                }
            }
        }
        logs.log(LogSeverity::Warn, "IMU MQTT disconnected; retrying");
        Timer::after_secs(2).await;
    }
}
