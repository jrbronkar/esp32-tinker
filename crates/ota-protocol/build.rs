use std::{fmt::Write as _, fs};

use protobuf::Message as _;

fn main() {
    println!("cargo:rerun-if-changed=../../protocol/ota.proto");
    println!("cargo:rerun-if-changed=../../protocol/management.proto");
    let out =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    let descriptor_path = out.join("management.fdset");
    let descriptors = protobuf_parse::Parser::new()
        .pure()
        .include("../../protocol")
        .inputs([
            "../../protocol/management.proto",
            "../../protocol/ota.proto",
        ])
        .file_descriptor_set()
        .expect("parse OTA and Management schemas");
    fs::write(
        &descriptor_path,
        descriptors.write_to_bytes().expect("encode descriptors"),
    )
    .expect("write descriptors");

    let mut paths = String::new();
    let management = descriptors
        .file
        .iter()
        .find(|file| file.name() == "management.proto")
        .expect("management.proto descriptor");
    for service in &management.service {
        for method in &service.method {
            writeln!(
                paths,
                r#"pub const {}_PATH: &str = "/{}.{}/{}";"#,
                method.name().to_ascii_uppercase(),
                management.package(),
                service.name(),
                method.name(),
            )
            .expect("write RPC path");
        }
    }
    fs::write(out.join("rpc_paths.rs"), paths).expect("write RPC paths");

    let mut generator = micropb_gen::Generator::new();
    generator
        .use_container_heapless_v0_8()
        .configure(".", micropb_gen::Config::new().max_bytes(64))
        .configure(
            ".ota.PartitionInfo.label",
            micropb_gen::Config::new().max_bytes(16),
        );
    generator.configure(
        ".ota.FirmwareChunk.data",
        micropb_gen::Config::new().max_bytes(4096),
    );
    generator.configure(
        ".ota.BeginUpdate.sha256",
        micropb_gen::Config::new().max_bytes(32),
    );
    generator.configure(
        ".ota.StatusResponse.management_state",
        micropb_gen::Config::new().max_bytes(16),
    );
    generator.configure(
        ".ota.ReadLogsResponse.entries",
        micropb_gen::Config::new().max_len(4),
    );
    generator.configure(
        ".ota.LogEntry.message",
        micropb_gen::Config::new().max_bytes(64),
    );
    generator
        .compile_fdset_file(&descriptor_path, out.join("ota.rs"))
        .expect("generate OTA messages");
}
