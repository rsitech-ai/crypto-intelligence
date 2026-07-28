//! Structural policy for generated protobuf bindings.

/// Returns the name of a generated public gRPC client module, if present.
pub fn grpc_client_module(source: &str) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_alphabetic() && bytes[index] != b'_' {
            index += 1;
            continue;
        }
        let start = index;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            index += 1;
        }
        let identifier = &source[start..index];
        if identifier.ends_with("_client") {
            let body = skip_trivia(bytes, index);
            if matches!(bytes.get(body), Some(b'{') | Some(b';')) {
                return Some(identifier);
            }
        }
    }
    None
}

fn skip_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes[index..].starts_with(b"//") {
            index += 2;
            while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            index = skip_nested_block_comment(bytes, index + 2);
        } else {
            return index;
        }
    }
}

fn skip_nested_block_comment(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1_usize;
    while index < bytes.len() && depth > 0 {
        if bytes[index..].starts_with(b"/*") {
            depth += 1;
            index += 2;
        } else if bytes[index..].starts_with(b"*/") {
            depth -= 1;
            index += 2;
        } else {
            index += 1;
        }
    }
    index
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

    #[test]
    fn multiline_generated_service_client_module_is_rejected() {
        let generated =
            "pub mod market_service_client\n{\n    pub struct MarketServiceClient<T>(T);\n}\n";
        assert_eq!(grpc_client_module(generated), Some("market_service_client"));
    }

    #[test]
    fn comments_and_raw_identifiers_do_not_hide_client_modules() {
        for generated in [
            "pub /* outer /* nested */ comment */ mod market_service_client {}\n",
            "pub // generated visibility\nmod market_service_client {}\n",
            "pub mod market_service_client /* generated body */ {}\n",
            "pub mod r#market_service_client {}\n",
        ] {
            assert_eq!(grpc_client_module(generated), Some("market_service_client"));
        }
    }

    #[test]
    fn preceding_literals_do_not_desynchronize_client_detection() {
        for generated in [
            "\"/*\"; pub mod market_service_client {}\n",
            "'/'; pub mod market_service_client {}\n",
            "r#\"/*\"#; pub mod market_service_client {}\n",
            "b\"/*\"; pub mod market_service_client {}\n",
            "br#\"/*\"#; pub mod market_service_client {}\n",
        ] {
            assert_eq!(grpc_client_module(generated), Some("market_service_client"));
        }
    }

    #[test]
    fn external_client_modules_are_rejected() {
        for generated in [
            "pub mod market_service_client;\n",
            "#[path = \"/private/tmp/generated-client.rs\"] pub mod market_service_client;\n",
        ] {
            assert_eq!(grpc_client_module(generated), Some("market_service_client"));
        }
    }
}
