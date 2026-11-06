use device_service::{DeviceLogs, LogSeverity};
use embassy_time::{Duration, Ticker};
use esp_hal::{
    gpio::Level,
    peripherals::{GPIO48, RMT},
    rmt::{Rmt, TxChannelConfig, TxChannelCreator},
    time::Rate,
};
use onboard_rgb::breathing_green;

#[embassy_executor::task]
pub async fn pulse(rmt: RMT<'static>, pin: GPIO48<'static>, logs: &'static DeviceLogs) {
    let Ok(rmt) = Rmt::new(rmt, Rate::from_mhz(80)) else {
        logs.log(LogSeverity::Warn, "RGB LED clock configuration failed");
        return;
    };
    let config = TxChannelConfig::default()
        .with_clk_divider(1)
        .with_idle_output_level(Level::Low)
        .with_idle_output(true);
    let Ok(channel) = rmt.channel0.configure_tx(&config) else {
        logs.log(LogSeverity::Warn, "RGB LED channel configuration failed");
        return;
    };
    let mut led = channel.with_pin(pin);
    let mut frame = 0_u16;
    let mut ticker = Ticker::every(Duration::from_millis(20));
    loop {
        let pulses = breathing_green(frame);
        let Ok(channel) = led.transmit(&pulses).and_then(|transfer| transfer.wait()) else {
            logs.log(LogSeverity::Warn, "RGB LED transmission failed");
            return;
        };
        led = channel;
        frame = frame.wrapping_add(1);
        ticker.next().await;
    }
}
