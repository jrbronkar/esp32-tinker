use micropb_gen::{ServiceGenerator, ServiceView, TypeResolver};
use proc_macro2::TokenStream;

struct ManagementPaths;

impl ServiceGenerator for ManagementPaths {
    fn generate(&mut self, service: &ServiceView<'_>, _: &dyn TypeResolver) -> TokenStream {
        // The transport handlers and client calls are handwritten. Validate the
        // signatures they implement before generating their shared RPC paths.
        let expected = [
            (
                "Status",
                ".ota.StatusRequest",
                ".ota.StatusResponse",
                false,
                false,
                "STATUS_PATH",
            ),
            (
                "Logs",
                ".ota.ReadLogsRequest",
                ".ota.ReadLogsResponse",
                false,
                true,
                "LOGS_PATH",
            ),
            (
                "Upload",
                ".ota.UploadRequest",
                ".ota.UploadAck",
                true,
                true,
                "UPLOAD_PATH",
            ),
        ];
        assert_eq!(service.name, "Management");
        assert_eq!(
            service.methods.len(),
            expected.len(),
            "update Rust routing with the Management RPCs"
        );
        let mut paths = TokenStream::new();
        for (name, input, output, client_streaming, server_streaming, constant) in expected {
            let method = service
                .methods
                .iter()
                .find(|method| method.name == name)
                .unwrap_or_else(|| panic!("missing Management RPC {name}"));
            assert_eq!(
                (
                    method.input_type,
                    method.output_type,
                    method.client_streaming,
                    method.server_streaming
                ),
                (input, output, client_streaming, server_streaming),
                "update the Rust handlers and clients for Management.{name}",
            );
            paths.extend(
                format!("pub const {constant}: &str = {:?};", method.full_path)
                    .parse::<TokenStream>()
                    .expect("valid Rust RPC path constant"),
            );
        }
        paths
    }
}

fn main() {
    println!("cargo:rerun-if-changed=../../protocol/ota.proto");
    println!("cargo:rerun-if-changed=../../protocol/management.proto");
    let mut generator = micropb_gen::Generator::new();
    generator
        .add_service_generator(ManagementPaths)
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
    generator.configure(
        ".ota.ReadLogsResponse.entries",
        micropb_gen::Config::new().max_len(4),
    );
    generator.configure(
        ".ota.LogEntry.message",
        micropb_gen::Config::new().max_bytes(64),
    );
    generator
        .compile_protos(
            &[
                "../../protocol/ota.proto",
                "../../protocol/management.proto",
            ],
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("ota.rs"),
        )
        .expect("generate OTA protocol (requires protoc on PATH)");
}
