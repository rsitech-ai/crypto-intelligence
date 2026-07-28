//! Structural policy for generated protobuf bindings.

/// Returns the name of a generated public gRPC client module, if present.
pub fn grpc_client_module(source: &str) -> Option<&str> {
    let bytes = source.as_bytes();
    let mut index = 0;
    while let Some((token, next)) = next_identifier(source, index) {
        index = next;
        if token != "pub" {
            continue;
        }
        let (keyword, after_keyword) = next_identifier(source, index)?;
        if keyword != "mod" {
            continue;
        }
        let (module, after_module) = next_identifier(source, after_keyword)?;
        let brace = skip_trivia(bytes, after_module);
        if bytes.get(brace) == Some(&b'{') && module.ends_with("_client") {
            return Some(module);
        }
    }
    None
}

fn next_identifier(source: &str, mut index: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    while index < bytes.len() {
        let after_trivia = skip_trivia(bytes, index);
        if after_trivia != index {
            index = after_trivia;
            continue;
        }
        if bytes[index..].starts_with(b"r#")
            && bytes
                .get(index + 2)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            index += 2;
            break;
        }
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            break;
        }
        index += 1;
    }
    let start = index;
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        index += 1;
    }
    (start != index).then(|| (&source[start..index], index))
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
}
