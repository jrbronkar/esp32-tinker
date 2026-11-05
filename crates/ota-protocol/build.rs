fn main() {
    println!("cargo:rerun-if-changed=../../protocol/ota.proto");
    println!("cargo:rerun-if-changed=../../protocol/management.proto");
    let out =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    let descriptors = out.join("management.fdset");
    protolink_grpc_gen::Generator::micropb()
        .with_include("../../protocol")
        .with_runtime_path("::protolink_grpc")
        .with_client(false)
        .with_blocking_client(false)
        .with_file_descriptor_set_path(&descriptors)
        .compile_protos(
            &[
                "../../protocol/management.proto",
                "../../protocol/ota.proto",
            ],
            out.join("management.rs"),
        )
        .expect("generate Management service");
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
        .compile_fdset_file(&descriptors, out.join("ota.rs"))
        .expect("generate OTA messages");
}
