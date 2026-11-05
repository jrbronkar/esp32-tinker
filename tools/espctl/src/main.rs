use std::{
    net::{IpAddr, SocketAddr},
    process::ExitCode,
    time::{Duration, Instant},
};

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Manage ESP32 status and development firmware updates",
    long_about = "Device status and profiling, plus unsigned development uploads over gRPC/HTTP2. Device identity is not authenticated. Uploads write the inactive slot; activation requires --commit."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Query one board's identity, firmware version, and uptime
    Status(StatusArgs),
    /// Read recent device management logs
    Logs(LogsArgs),
    /// Discover ESP32 management services on the same IPv4 LAN
    Discover(DiscoverArgs),
    /// Sample fresh connections and summarize host-observed query phases
    Profile(ProfileArgs),
    /// Upload unsigned development firmware; activation requires --commit
    Upload(UploadArgs),
}

#[derive(Args)]
struct LogsArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Stream until interrupted
    #[arg(long)]
    follow: bool,
}

#[derive(Args)]
struct DiscoverArgs {
    /// Restrict mDNS browsing to the interface with this local IPv4 address
    #[arg(long)]
    interface: Option<std::net::Ipv4Addr>,
    /// Collection window in milliseconds
    #[arg(long, default_value_t = espctl::discovery::DEFAULT_SCAN_MS, value_parser = clap::value_parser!(u64).range(espctl::discovery::MIN_SCAN_MS..=espctl::discovery::MAX_SCAN_MS))]
    timeout_ms: u64,
    /// Repeat discovery until interrupted; each view contains only that scan's responders
    #[arg(long)]
    watch: bool,
}

#[derive(Args)]
struct StatusArgs {
    #[command(flatten)]
    target: TargetArgs,

    /// Show connection, retry, and response timing breakdown
    #[arg(long)]
    timings: bool,

    /// Inspect configured flash capacity and running/OTA partitions (read-only)
    #[arg(long)]
    boot: bool,
}

#[derive(Args)]
struct ProfileArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Number of queries (each opens a new connection)
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=1000))]
    samples: u32,
    /// Pause between completed queries, in milliseconds
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u64).range(0..=60000))]
    interval_ms: u64,
    /// Print per-sample CSV instead of summary statistics
    #[arg(long)]
    csv: bool,
}

#[derive(Args)]
struct UploadArgs {
    /// Stream management logs over the same HTTP/2 connection during upload
    #[arg(long)]
    logs: bool,
    #[command(flatten)]
    target: TargetArgs,
    /// Application-only .bin exported by espflash (not ELF or a merged image)
    #[arg(long)]
    image: std::path::PathBuf,
    /// Must match the firmware's embedded FIRMWARE_VERSION
    #[arg(long)]
    firmware_version: String,
    /// Activate the verified image and reboot; otherwise leave the current image selected
    #[arg(long)]
    commit: bool,
}

#[derive(Args)]
struct TargetArgs {
    /// Device IPv4 or IPv6 address (not a URL or hostname)
    #[arg(long, value_name = "IP")]
    host: IpAddr,

    /// gRPC management port
    #[arg(long, default_value_t = ota_protocol::PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// Status query deadline or upload I/O timeout in ms (upload total limit: 180 s)
    #[arg(long, value_name = "MS", default_value_t = ota_protocol::SESSION_TIMEOUT_SECS * 1000, value_parser = clap::value_parser!(u64).range(1..=60000))]
    timeout_ms: u64,
}

fn run(command: Commands) -> Result<(), String> {
    match command {
        Commands::Status(args) => run_status(args),
        Commands::Logs(args) => run_logs(args),
        Commands::Discover(args) => run_discover(args),
        Commands::Profile(args) => run_profile(args),
        Commands::Upload(args) => {
            let address = SocketAddr::new(args.target.host, args.target.port);
            let timeout = Duration::from_millis(args.target.timeout_ms);
            let result = if args.logs {
                espctl::upload::upload_with_logs(
                    address,
                    timeout,
                    &args.image,
                    &args.firmware_version,
                    args.commit,
                    &mut |batch| match batch {
                        Ok(batch) => {
                            if batch.reset {
                                eprintln!("Log cursor reset.");
                            }
                            if batch.dropped {
                                eprintln!("Older log entries were overwritten.");
                            }
                            for entry in batch.entries {
                                eprintln!(
                                    "{} {:>10} {:?} {}",
                                    entry.sequence,
                                    entry.uptime_ms,
                                    entry.severity,
                                    entry.message.escape_debug()
                                );
                            }
                        }
                        Err(error) => eprintln!("Log stream unavailable: {}", error.escape_debug()),
                    },
                )
            } else {
                espctl::upload::upload(
                    address,
                    timeout,
                    &args.image,
                    &args.firmware_version,
                    args.commit,
                )
            }
            .map_err(|e| e.to_string())?;
            println!(
                "Verified {} bytes in slot at 0x{:x}",
                result.bytes, result.target_offset
            );
            println!(
                "{}",
                if result.committed {
                    "Committed; board is rebooting. Reconnect and verify its firmware version."
                } else {
                    "Not activated; current firmware remains selected."
                }
            );
            Ok(())
        }
    }
}

fn run_discover(args: DiscoverArgs) -> Result<(), String> {
    loop {
        let result =
            espctl::discovery::discover(args.interface, Duration::from_millis(args.timeout_ms))
                .map_err(|e| format!("discovery: {e}"))?;
        if result.devices.is_empty() {
            println!(
                "No devices responded in this scan (multicast filtering or LAN isolation can hide devices)."
            );
        } else {
            println!(
                "{:<34} {:<22} {:<22} {:<14} OTA",
                "Device", "Management (gRPC)", "Firmware", "Advertised state"
            );
            for device in result.devices {
                println!(
                    "{:<34} {:<22} {:<22} {:<14} {}",
                    device.device_id.escape_debug().to_string(),
                    device.address,
                    device.firmware.escape_debug().to_string(),
                    device.management_state.escape_debug().to_string(),
                    if device.ota_available { "yes" } else { "no" }
                );
            }
        }
        if result.limit_reached {
            eprintln!(
                "Discovery result limit reached ({} devices); additional replies omitted.",
                espctl::discovery::MAX_DEVICES
            );
        }
        if !args.watch {
            return Ok(());
        }
        println!();
        std::thread::sleep(espctl::discovery::WATCH_INTERVAL);
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn run_status(args: StatusArgs) -> Result<(), String> {
    let address = SocketAddr::new(args.target.host, args.target.port);
    let started = Instant::now();
    let result = espctl::query_status_with_options(
        address,
        Duration::from_millis(args.target.timeout_ms),
        args.timings,
        args.boot,
    );
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let query =
        result.map_err(|e| format!("status from {address} after {latency_ms:.2} ms: {e}"))?;
    let status = query.status;
    // Escape untrusted text so a remote endpoint cannot inject terminal controls.
    println!("Device:      {}", status.device_id.escape_debug());
    println!("Address:     {address}");
    println!("Chip:        {}", status.chip.escape_debug());
    println!("Application: {}", status.application.escape_debug());
    println!("Firmware:    {}", status.firmware_version.escape_debug());
    if !status.management_state.is_empty() {
        println!("State:       {}", status.management_state.escape_debug());
    }
    println!("Uptime:      {} ms", status.uptime_ms);
    println!("Latency:     {latency_ms:.2} ms (total query)");
    println!(
        "OTA:         {}",
        if status.ota_available {
            "available"
        } else {
            "unavailable (unsupported boot layout)"
        }
    );
    if args.boot {
        if let Some(boot) = status.boot() {
            println!(
                "Boot layout: {}",
                if boot.layout_valid {
                    "read successfully"
                } else {
                    "unavailable/invalid"
                }
            );
            println!(
                "Configured flash: {} bytes (image header)",
                boot.configured_flash_size_bytes
            );
            println!(
                "Partitions: {}; OTA app slots: {}; OTA metadata: {}",
                boot.partition_count, boot.ota_app_partition_count, boot.has_ota_data
            );
            for (name, part) in [
                ("Running", boot.running()),
                ("OTA 0", boot.ota_0()),
                ("OTA 1", boot.ota_1()),
            ] {
                if let Some(part) = part {
                    println!(
                        "{name}: {} offset=0x{:x} size=0x{:x}",
                        part.label.escape_debug(),
                        part.offset,
                        part.size
                    );
                } else {
                    println!("{name}: unavailable");
                }
            }
            if !boot.error.is_empty() {
                println!("Boot inspection error: {}", boot.error.escape_debug());
            }
            println!(
                "Automatic rollback support: unverified (slot selection is verified separately)"
            );
        } else {
            println!("Boot inventory unavailable; flash the boot-inspection device-demo");
        }
    }
    if args.timings {
        if let Some(device) = status.timings() {
            println!("Device decode: {} us", device.decode_us);
            println!("Device prepare: {} us", device.prepare_us);
        } else {
            println!("Device timings unavailable; flash the instrumented device-demo");
        }
        let t = query.timings;
        println!(
            "Connect:     {:.3} ms (includes {:.3} ms retry wait; {} refusals)",
            ms(t.connect),
            ms(t.retry_wait),
            t.refused_connects
        );
        println!("Encode:      {:.3} ms", ms(t.encode));
        println!(
            "RPC response:{:.3} ms (send, server work, body and trailers)",
            ms(t.rpc)
        );
        println!("Decode/check:{:.3} ms", ms(t.decode_validate));
    }
    Ok(())
}

fn run_profile(args: ProfileArgs) -> Result<(), String> {
    let address = SocketAddr::new(args.target.host, args.target.port);
    let mut samples = Vec::new();
    let mut failed = 0;
    let mut device_timings = Vec::new();
    if args.csv {
        println!(
            "sample,total_ms,connect_ms,retry_wait_ms,encode_ms,rpc_ms,decode_validate_ms,refused_connects,device_decode_us,device_prepare_us"
        );
    }
    for index in 1..=args.samples {
        match espctl::query_status_profiled(address, Duration::from_millis(args.target.timeout_ms))
        {
            Ok(mut query) => {
                let t = query.timings;
                if args.csv {
                    println!(
                        "{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{},{},{}",
                        index,
                        ms(t.total),
                        ms(t.connect),
                        ms(t.retry_wait),
                        ms(t.encode),
                        ms(t.rpc),
                        ms(t.decode_validate),
                        t.refused_connects,
                        query
                            .status
                            .timings()
                            .map(|d| d.decode_us.to_string())
                            .unwrap_or_default(),
                        query
                            .status
                            .timings()
                            .map(|d| d.prepare_us.to_string())
                            .unwrap_or_default()
                    );
                }
                if let Some(device) = query.status.take_timings() {
                    device_timings.push(device);
                }
                samples.push(t);
            }
            Err(error) => {
                failed += 1;
                eprintln!("sample {index}: {error}");
            }
        }
        if index < args.samples {
            std::thread::sleep(Duration::from_millis(args.interval_ms));
        }
    }
    if !args.csv && !samples.is_empty() {
        println!(
            "Profile {address}: {} successful, {failed} failed (fresh TCP connection per query)",
            samples.len()
        );
        println!(
            "{:<15} {:>10} {:>10} {:>10} {:>10} {:>10}",
            "Phase (ms)", "min", "median", "p95", "max", "mean"
        );
        for (name, values) in [
            (
                "total",
                samples.iter().map(|t| ms(t.total)).collect::<Vec<_>>(),
            ),
            ("connect", samples.iter().map(|t| ms(t.connect)).collect()),
            (
                "retry wait*",
                samples.iter().map(|t| ms(t.retry_wait)).collect(),
            ),
            ("encode", samples.iter().map(|t| ms(t.encode)).collect()),
            ("rpc", samples.iter().map(|t| ms(t.rpc)).collect()),
            (
                "decode/check",
                samples.iter().map(|t| ms(t.decode_validate)).collect(),
            ),
        ] {
            let mut values = values;
            values.sort_by(f64::total_cmp);
            let n = values.len();
            let median = (values[(n - 1) / 2] + values[n / 2]) / 2.0;
            let p95 = values[(95 * n).div_ceil(100) - 1];
            let mean = values.iter().sum::<f64>() / n as f64;
            println!(
                "{name:<15} {:>10.3} {median:>10.3} {p95:>10.3} {:>10.3} {mean:>10.3}",
                values[0],
                values[n - 1]
            );
        }
        if device_timings.is_empty() {
            println!("Device timings unavailable; flash the instrumented device-demo.");
        } else {
            println!(
                "Device timings ({} responses; microseconds):",
                device_timings.len()
            );
            for (name, mut values) in [
                (
                    "decode",
                    device_timings
                        .iter()
                        .map(|d| d.decode_us)
                        .collect::<Vec<_>>(),
                ),
                (
                    "prepare",
                    device_timings.iter().map(|d| d.prepare_us).collect(),
                ),
            ] {
                values.sort_unstable();
                let median =
                    (values[(values.len() - 1) / 2] as f64 + values[values.len() / 2] as f64) / 2.0;
                println!("  {name}: median {median:.1} us");
            }
        }
        println!(
            "* Retry wait is included in connect. Refused connects: {}. Host timing, not device CPU time.",
            samples.iter().map(|t| t.refused_connects).sum::<u32>()
        );
    }
    if failed > 0 {
        return Err(format!("{failed}/{} profile queries failed", args.samples));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run_logs(args: LogsArgs) -> Result<(), String> {
    if args.follow {
        eprintln!("Following management logs; reconnecting after device resets or network loss.");
    }
    let mut cursor = ota_protocol::ReadLogsRequest::default();
    loop {
        let result = espctl::stream_logs(
            SocketAddr::new(args.target.host, args.target.port),
            Duration::from_millis(args.target.timeout_ms),
            args.follow,
            &mut cursor,
            |logs| {
                if logs.reset {
                    eprintln!("Device boot or cursor changed; log cursor reset.");
                }
                if logs.dropped {
                    eprintln!("Older log entries were overwritten.");
                }
                for entry in &logs.entries {
                    println!(
                        "{} {:>10} {:?} {}",
                        entry.sequence,
                        entry.uptime_ms,
                        entry.severity,
                        entry.message.escape_debug()
                    );
                }
            },
        );
        match result {
            Ok(()) if !args.follow => return Ok(()),
            Err(error) if !args.follow || error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(error.to_string());
            }
            Err(error) => eprintln!("Logs unavailable: {error}; retrying."),
            Ok(()) => {}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
