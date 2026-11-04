fn main() {
    println!("cargo:rerun-if-changed=../../protocol/ota.proto");
    let mut generator = micropb_gen::Generator::new();
    generator
        .use_container_heapless()
        .add_protoc_arg("-I../../protocol")
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
    generator
        .compile_protos(
            &["../../protocol/ota.proto"],
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("ota.rs"),
        )
        .expect("generate OTA protocol (requires protoc on PATH)");
}
