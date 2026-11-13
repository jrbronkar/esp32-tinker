use std::{fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../../protocol/management.proto");
    println!("cargo:rerun-if-changed=../../protocol/ota.proto");
    let descriptors = protobuf_parse::Parser::new()
        .pure()
        .include("../../protocol")
        .input("../../protocol/management.proto")
        .file_descriptor_set()
        .expect("parse Management schema");
    let schema = descriptors
        .file
        .iter()
        .find(|file| file.name() == "management.proto")
        .expect("management.proto descriptor");
    let bindings = embedded_grpc_build::generate_with_packages(
        schema,
        "ota_protocol::UPLOAD_FRAME",
        &[("ota", "ota_protocol::ota_")],
    )
    .expect("generate Management bindings");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    fs::write(out.join("management.rs"), bindings).expect("write Management bindings");
}
