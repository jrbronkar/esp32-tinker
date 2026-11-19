use core::cell::Cell;

use device_service::{DeviceLogs, LogSeverity};
use embassy_net::{tcp::TcpSocket, Stack};
use embassy_time::{with_timeout, Duration, Ticker, Timer};
use heapless::String;
use imu_telemetry::{fusion::MagneticSample, OrientationSample};
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
    pub orientation: Cell<Option<OrientationSample>>,
    pub magnetic: Cell<Option<MagneticSample>>,
}

pub(super) async fn run(stack: Stack<'static>, logs: &DeviceLogs, samples: &Samples) {
    let Some(settings) = firmware_config::IMU_MQTT else {
        logs.log(
            LogSeverity::Warn,
            "Configure telemetry.imu.mqtt in firmware.yaml",
        );
        return;
    };
    let endpoint = settings.broker;
    let device = settings.device_id;
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
                let publication =
                    PublicationOptions::new(TopicReference::Name(topic.as_borrowed()));
                let mut ticker = Ticker::every(Duration::from_millis(20));
                loop {
                    ticker.next().await;
                    let Some(orientation) = samples.orientation.take() else {
                        continue;
                    };
                    let Ok(payload) = imu_telemetry::encode(&orientation) else {
                        logs.log(LogSeverity::Error, "IMU orientation encoding failed");
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
