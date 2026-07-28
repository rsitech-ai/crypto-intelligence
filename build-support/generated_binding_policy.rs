//! Structural policy for generated protobuf bindings.

/// Returns the name of a generated public gRPC client module, if present.
pub fn grpc_client_module(source: &str) -> Option<&str> {
    source.lines().find_map(|line| {
        let declaration = line.trim().strip_prefix("pub mod ")?.trim();
        let module = declaration.strip_suffix('{')?.trim();
        module.ends_with("_client").then_some(module)
    })
}

#[cfg(test)]
mod tests {
    use super::grpc_client_module;

    #[test]
    fn harmless_client_named_fields_are_allowed() {
        let generated =
            "pub struct Session { pub client_id: String, pub client_version: String }\n";
        assert_eq!(grpc_client_module(generated), None);
    }

    #[test]
    fn generated_service_client_module_is_rejected() {
        let generated =
            "pub mod market_service_client {\n    pub struct MarketServiceClient<T>;\n}\n";
        assert_eq!(grpc_client_module(generated), Some("market_service_client"));
    }
}
