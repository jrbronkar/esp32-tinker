use std::{fmt::Write as _, fs, path::PathBuf};

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
    let mut bindings = String::new();

    for service in &schema.service {
        let name = service.name();
        let methods = service
            .method
            .iter()
            .filter(|method| !method.client_streaming() && !method.server_streaming());
        writeln!(bindings, "#[allow(async_fn_in_trait)]\npub trait {name} {{").unwrap();
        for method in methods.clone() {
            let rpc = rust_method(method.name());
            let input = ota_type(method.input_type());
            let output = ota_type(method.output_type());
            writeln!(
                bindings,
                "async fn {rpc}(&mut self, request: {input}) -> Result<{output}, Status>;"
            )
            .unwrap();
        }
        writeln!(
            bindings,
            "}}
            pub struct {name}Server<S> {{ service: S }}
            impl<S> {name}Server<S> {{
                pub fn new(service: S) -> Self {{ Self {{ service }} }}
            }}
            impl<S: {name}> embedded_grpc::Service for {name}Server<S> {{
                async fn call(&mut self, request: embedded_grpc::HttpRequest) -> embedded_grpc::HttpResponse {{
                    let path = match embedded_grpc::rpc_path(&request) {{
                        Ok(path) => path,
                        Err(response) => return response,
                    }};
                    match path {{"
        )
        .unwrap();
        for method in methods {
            let rpc = rust_method(method.name());
            let input = ota_type(method.input_type());
            let path = format!("/{}.{name}/{}", schema.package(), method.name());
            writeln!(
                bindings,
                "{path:?} => {{
                    let message = match embedded_grpc::decode_unary::<{input}, {{ ota_protocol::MAX_FRAME }}>(&request) {{
                        Ok(message) => message,
                        Err(status) => return status.into_response(),
                    }};
                    embedded_grpc::unary_response::<_, {{ ota_protocol::MAX_FRAME }}>(self.service.{rpc}(message).await)
                }},"
            )
            .unwrap();
        }
        bindings.push_str("_ => Status::Unimplemented.into_response(),\n}\n}\n}\n");
    }

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output directory"));
    fs::write(out.join("management.rs"), bindings).expect("write Management bindings");
}

fn ota_type(proto: &str) -> String {
    let message = proto
        .strip_prefix(".ota.")
        .expect("expected an OTA message");
    assert!(!message.contains('.'), "expected a top-level OTA message");
    format!("ota_protocol::ota_::{message}")
}

fn rust_method(proto: &str) -> String {
    let mut name = String::new();
    for (index, letter) in proto.chars().enumerate() {
        if index > 0 && letter.is_ascii_uppercase() {
            name.push('_');
        }
        name.push(letter.to_ascii_lowercase());
    }
    name
}
