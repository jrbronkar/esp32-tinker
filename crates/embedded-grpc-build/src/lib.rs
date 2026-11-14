//! Host-side protobuf service bindings for the embedded gRPC runtime.

use protobuf::descriptor::FileDescriptorProto;
use std::fmt::Write as _;

/// Generate bindings for top-level messages mapped by protobuf package.
pub fn generate_with_packages(
    schema: &FileDescriptorProto,
    max_message_len: &str,
    packages: &[(&str, &str)],
) -> Result<String, String> {
    generate(schema, max_message_len, |name| {
        let (package, message) = name
            .strip_prefix('.')
            .and_then(|name| name.rsplit_once('.'))
            .unwrap_or(("", name.trim_start_matches('.')));
        packages
            .iter()
            .find(|(proto_package, _)| *proto_package == package)
            .map(|(_, rust_module)| format!("{rust_module}::{message}"))
            .ok_or_else(|| format!("no Rust type mapping for {name}"))
    })
}

/// Generate traits and server dispatch for all services in a descriptor.
/// Resolve fully qualified protobuf message names to Rust type paths. The
/// message limit is a Rust const expression available to the generated code.
pub fn generate(
    schema: &FileDescriptorProto,
    max_message_len: &str,
    resolve: impl Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut bindings = String::new();

    for service in &schema.service {
        let name = service.name();
        for method in &service.method {
            if method.client_streaming() && !method.server_streaming() {
                return Err(format!(
                    "{name}.{} uses unsupported client streaming with a unary response",
                    method.name()
                ));
            }
        }
        let methods = service
            .method
            .iter()
            .filter(|method| !method.client_streaming() && !method.server_streaming());
        let server_streaming = service
            .method
            .iter()
            .filter(|method| !method.client_streaming() && method.server_streaming());
        let bidirectional = service
            .method
            .iter()
            .filter(|method| method.client_streaming() && method.server_streaming());
        writeln!(bindings, "#[allow(async_fn_in_trait)]\npub trait {name} {{").unwrap();
        for method in methods.clone() {
            let rpc = rust_method(method.name());
            let input = resolve(method.input_type())?;
            let output = resolve(method.output_type())?;
            writeln!(
                bindings,
                "async fn {rpc}(&self, request: {input}) -> Result<{output}, embedded_grpc::Status>;"
            )
            .unwrap();
        }
        for method in server_streaming.clone() {
            let rpc = rust_method(method.name());
            let input = resolve(method.input_type())?;
            let output = resolve(method.output_type())?;
            writeln!(
                bindings,
                "async fn {rpc}(&self, request: {input}) -> Result<impl embedded_grpc::Stream<Item = Result<{output}, embedded_grpc::Status>>, embedded_grpc::Status>;"
            )
            .unwrap();
        }
        for method in bidirectional.clone() {
            let rpc = rust_method(method.name());
            let input = resolve(method.input_type())?;
            let output = resolve(method.output_type())?;
            writeln!(
                bindings,
                "async fn {rpc}(&self, request: embedded_grpc::Request<embedded_grpc::Streaming<{input}, {{ {max_message_len} }}>>) -> Result<embedded_grpc::Response<impl embedded_grpc::Stream<Item = Result<{output}, embedded_grpc::Status>>>, embedded_grpc::Status>;"
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
                async fn call(
                    &mut self,
                    path: &str,
                    request: embedded_grpc::UnaryRequest<'_>,
                ) -> Result<embedded_grpc::UnaryResponse, embedded_grpc::Status> {{
                    match path {{"
        )
        .unwrap();
        for method in methods.clone() {
            let rpc = rust_method(method.name());
            let input = resolve(method.input_type())?;
            let path = rpc_path(schema.package(), name, method.name());
            writeln!(
                bindings,
                "{path:?} => {{
                    let message = request.decode::<{input}, {{ {max_message_len} }}>()?;
                    let reply = self.service.{rpc}(message).await?;
                    embedded_grpc::UnaryResponse::encode::<_, {{ {max_message_len} }}>(reply)
                }},"
            )
            .unwrap();
        }
        bindings.push_str("_ => Err(embedded_grpc::Status::Unimplemented),\n}\n}\n}\n");
        writeln!(
            bindings,
            "impl<S: {name}> embedded_grpc::StreamingService for {name}Server<S> {{
                async fn call(
                    &self,
                    path: &str,
                    call: embedded_grpc::StreamingCall<'_>,
                ) -> Result<(), embedded_grpc::StreamError> {{
                    match path {{"
        )
        .unwrap();
        for method in methods.chain(server_streaming) {
            let rpc = rust_method(method.name());
            let input = resolve(method.input_type())?;
            let path = rpc_path(schema.package(), name, method.name());
            let send = if method.server_streaming() {
                "stream"
            } else {
                "reply"
            };
            writeln!(
                bindings,
                "{path:?} => {{
                    let Some((request, mut response)) = call.unary::<{input}, {{ {max_message_len} }}>().await? else {{
                        return Ok(());
                    }};
                    match response.start(self.service.{rpc}(request)).await {{
                        Ok(Some(Ok(value))) => response.{send}::<_, {{ {max_message_len} }}>(value).await,
                        Ok(Some(Err(status))) | Err(status) => response.reject(status).await,
                        Ok(None) => Ok(()),
                    }}
                }},"
            )
            .unwrap();
        }
        for method in bidirectional {
            let rpc = rust_method(method.name());
            let input = resolve(method.input_type())?;
            let path = rpc_path(schema.package(), name, method.name());
            writeln!(
                bindings,
                "{path:?} => {{
                    let (request, mut response) = call.streaming::<{input}, {{ {max_message_len} }}>();
                    match response.start(self.service.{rpc}(request)).await {{
                        Ok(Some(Ok(value))) => response.stream::<_, {{ {max_message_len} }}>(value.into_inner()).await,
                        Ok(Some(Err(status))) | Err(status) => response.reject(status).await,
                        Ok(None) => Ok(()),
                    }}
                }},"
            )
            .unwrap();
        }
        bindings
            .push_str("_ => call.reject(embedded_grpc::Status::Unimplemented).await,\n}\n}\n}\n");
    }

    Ok(bindings)
}

fn rpc_path(package: &str, service: &str, method: &str) -> String {
    if package.is_empty() {
        format!("/{service}/{method}")
    } else {
        format!("/{package}.{service}/{method}")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::descriptor::{MethodDescriptorProto, ServiceDescriptorProto};

    #[test]
    fn maps_imported_messages_and_uses_descriptor_rpc_paths() -> Result<(), String> {
        let mut method = MethodDescriptorProto::new();
        method.set_name("Watch".into());
        method.set_input_type(".example.messages.Outer.Request".into());
        method.set_output_type(".other.Reply".into());
        method.set_server_streaming(true);
        let mut service = ServiceDescriptorProto::new();
        service.set_name("Example".into());
        service.method.push(method);
        let mut schema = FileDescriptorProto::new();
        schema.set_package("example.v1".into());
        schema.service.push(service);
        let generate_bindings = |schema: &FileDescriptorProto| {
            generate(schema, "crate::LIMIT", |name| match name {
                ".example.messages.Outer.Request" => Ok("crate::nested::Request".into()),
                ".other.Reply" => Ok("other::Reply".into()),
                _ => Err(format!("unmapped message {name}")),
            })
        };
        let bindings = generate_bindings(&schema)?;
        assert!(bindings.contains("/example.v1.Example/Watch"));
        assert!(bindings.contains("request: crate::nested::Request"));
        assert!(bindings.contains("Result<other::Reply, embedded_grpc::Status>"));
        assert!(bindings.contains("{ crate::LIMIT }"));

        let packages = [
            ("example.messages.Outer", "crate::nested"),
            ("other", "other"),
        ];
        assert_eq!(
            generate_with_packages(&schema, "crate::LIMIT", &packages)?,
            bindings,
        );
        assert!(generate_with_packages(&schema, "crate::LIMIT", &packages[..1]).is_err());

        schema.clear_package();
        assert!(generate_bindings(&schema)?.contains("/Example/Watch"));
        let method = &mut schema.service[0].method[0];
        method.set_server_streaming(false);
        method.set_client_streaming(true);
        assert!(generate_bindings(&schema).is_err());
        Ok(())
    }
}
