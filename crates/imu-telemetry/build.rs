use std::{env, fs, path::PathBuf};

use protobuf::Message;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = "../../protocol/imu_telemetry.proto";
    println!("cargo:rerun-if-changed={schema}");
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing Cargo output directory")?);
    let descriptors = protobuf_parse::Parser::new()
        .pure()
        .include("../../protocol")
        .input(schema)
        .file_descriptor_set()?;
    let descriptor_path = out.join("imu_telemetry.fdset");
    fs::write(&descriptor_path, descriptors.write_to_bytes()?)?;
    let mut generator = micropb_gen::Generator::new();
    generator.use_container_heapless_v0_8();
    generator.compile_fdset_file(descriptor_path, out.join("imu_telemetry.rs"))?;
    Ok(())
}
