use std::{
    env, fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
};

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    wifi: Wifi,
    firmware: Firmware,
    #[serde(default)]
    telemetry: Telemetry,
    #[serde(default)]
    diagnostics: Diagnostics,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wifi {
    ssid: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Firmware {
    version: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Telemetry {
    #[serde(default)]
    imu: ImuTelemetry,
    #[serde(default)]
    soil: SoilTelemetry,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImuTelemetry {
    udp_host: Option<Ipv4Addr>,
    mqtt: Option<Mqtt>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoilTelemetry {
    udp_host: Option<Ipv4Addr>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mqtt {
    broker: SocketAddrV4,
    device_id: String,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Diagnostics {
    ota_heap_reserve_bytes: u32,
}

impl Config {
    fn validate(&self) -> Result<(), &'static str> {
        if !(1..=32).contains(&self.wifi.ssid.len()) {
            return Err("wifi.ssid must contain 1–32 bytes");
        }
        if self.wifi.password.len() > 64 {
            return Err("wifi.password must contain at most 64 bytes");
        }
        let version = self.firmware.version.as_bytes();
        if !(1..=31).contains(&version.len())
            || !version[0].is_ascii_alphanumeric()
            || !version
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
        {
            return Err("firmware.version must be 1–31 ASCII letters, digits, dots, underscores or hyphens and start with a letter or digit");
        }
        if let Some(mqtt) = &self.telemetry.imu.mqtt {
            if mqtt.broker.port() == 0 {
                return Err("telemetry.imu.mqtt.broker must specify a nonzero TCP port");
            }
            if !(1..=48).contains(&mqtt.device_id.len())
                || !mqtt
                    .device_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err("telemetry.imu.mqtt.device_id must be 1–48 ASCII letters, digits, underscores or hyphens");
            }
        }
        Ok(())
    }

    fn rust_constants(&self) -> String {
        fn ipv4(address: Ipv4Addr) -> String {
            let [a, b, c, d] = address.octets();
            format!("Ipv4Addr::new({a}, {b}, {c}, {d})")
        }

        fn optional_ipv4(address: Option<Ipv4Addr>) -> String {
            address.map_or_else(|| "None".to_owned(), |ip| format!("Some({})", ipv4(ip)))
        }

        let imu = &self.telemetry.imu;
        let mqtt =
            imu.mqtt.as_ref().map_or_else(
                || "None".to_owned(),
                |mqtt| {
                    format!(
                    "Some(MqttSettings {{ broker: SocketAddrV4::new({}, {}), device_id: {:?} }})",
                    ipv4(*mqtt.broker.ip()), mqtt.broker.port(), mqtt.device_id
                )
                },
            );
        format!(
            "pub const WIFI_SSID: &str = {:?};\n\
             pub const WIFI_PASSWORD: &str = {:?};\n\
             pub const VERSION: &str = {:?};\n\
             pub const IMU_UDP_HOST: Option<Ipv4Addr> = {};\n\
             pub const SOIL_UDP_HOST: Option<Ipv4Addr> = {};\n\
             pub const IMU_MQTT: Option<MqttSettings> = {mqtt};\n\
             pub const OTA_HEAP_RESERVE_BYTES: usize = {};\n",
            self.wifi.ssid,
            self.wifi.password,
            self.firmware.version,
            optional_ipv4(imu.udp_host),
            optional_ipv4(self.telemetry.soil.udp_host),
            self.diagnostics.ota_heap_reserve_bytes,
        )
    }
}

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-env-changed=FIRMWARE_CONFIG");
    let path = match env::var_os("FIRMWARE_CONFIG") {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("FIRMWARE_CONFIG must be an absolute path".to_owned());
            }
            path
        }
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../firmware.yaml"),
    };
    println!("cargo:rerun-if-changed={}", path.display());
    let source = fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read {}: {error}. Copy firmware.example.yaml to firmware.yaml and edit it",
            path.display()
        )
    })?;
    let config: Config = serde_yaml::from_str(&source).map_err(|error| {
        // Parser errors can contain credentials from the input.
        let location = error.location().map_or_else(String::new, |location| {
            format!(
                " near line {}, column {}",
                location.line(),
                location.column()
            )
        });
        format!(
            "invalid firmware settings in {}{location}; see firmware.example.yaml",
            path.display()
        )
    })?;
    config
        .validate()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing Cargo output directory")?);
    fs::write(out.join("settings.rs"), config.rust_constants()).map_err(|error| error.to_string())
}
