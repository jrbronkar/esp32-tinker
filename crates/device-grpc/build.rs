fn main() {
    println!("cargo:rerun-if-changed=../../protocol/management.proto");
    println!("cargo:rerun-if-changed=../../protocol/ota.proto");
    let out =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    protolink_grpc_gen::Generator::micropb()
        .with_include("../../protocol")
        .with_client(false)
        .with_blocking_client(false)
        .compile_protos(
            &["../../protocol/management.proto"],
            out.join("management.rs"),
        )
        .expect("generate Management service");
}
