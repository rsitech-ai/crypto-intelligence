//! Structural policy for generated protobuf bindings.

/// Returns the name of a generated public gRPC client module, if present.
pub fn grpc_client_module(source: &str) -> Option<&str> {
    let mut tokens = RustTokens::new(source);
    while let Some(token) = tokens.next() {
        if token != Token::Identifier("mod") {
            continue;
        }
        let Some(Token::Identifier(identifier)) = tokens.next() else {
            continue;
        };
        let Some(Token::Punctuation(terminator)) = tokens.next() else {
            continue;
        };
        if identifier.ends_with("_client") && matches!(terminator, b'{' | b';') {
            return Some(identifier);
        }
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Token<'a> {
    Identifier(&'a str),
    Punctuation(u8),
}

struct RustTokens<'a> {
    source: &'a str,
    index: usize,
}

impl<'a> RustTokens<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, index: 0 }
    }

    fn next(&mut self) -> Option<Token<'a>> {
        let bytes = self.source.as_bytes();
        loop {
            while bytes.get(self.index).is_some_and(u8::is_ascii_whitespace) {
                self.index += 1;
            }
            if self.index >= bytes.len() {
                return None;
            }

            if bytes[self.index..].starts_with(b"//") {
                self.index += 2;
                while bytes.get(self.index).is_some_and(|byte| *byte != b'\n') {
                    self.index += 1;
                }
                continue;
            }
            if bytes[self.index..].starts_with(b"/*") {
                self.index = skip_nested_block_comment(bytes, self.index + 2);
                continue;
            }
            if let Some(end) = literal_end(self.source, self.index) {
                self.index = end;
                continue;
            }
            if bytes[self.index..].starts_with(b"r#")
                && bytes
                    .get(self.index + 2)
                    .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            {
                let start = self.index + 2;
                self.index = identifier_end(bytes, start);
                return Some(Token::Identifier(&self.source[start..self.index]));
            }
            if bytes[self.index].is_ascii_alphabetic() || bytes[self.index] == b'_' {
                let start = self.index;
                self.index = identifier_end(bytes, start);
                return Some(Token::Identifier(&self.source[start..self.index]));
            }

            let punctuation = bytes[self.index];
            self.index += 1;
            return Some(Token::Punctuation(punctuation));
        }
    }
}

fn identifier_end(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        index += 1;
    }
    index
}

fn literal_end(source: &str, index: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    for prefix in [b"br".as_slice(), b"cr".as_slice(), b"r".as_slice()] {
        if bytes[index..].starts_with(prefix)
            && let Some(end) = raw_string_end(bytes, index + prefix.len())
        {
            return Some(end);
        }
    }

    for prefix in [b"b".as_slice(), b"c".as_slice(), b"".as_slice()] {
        if bytes[index..].starts_with(prefix) && bytes.get(index + prefix.len()) == Some(&b'"') {
            return Some(quoted_string_end(bytes, index + prefix.len()));
        }
    }

    if bytes[index..].starts_with(b"b'") {
        return char_literal_end(source, index + 1);
    }
    if bytes[index] == b'\'' {
        return char_literal_end(source, index);
    }
    None
}

fn raw_string_end(bytes: &[u8], mut index: usize) -> Option<usize> {
    let hashes_start = index;
    while bytes.get(index) == Some(&b'#') {
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    let hashes = index - hashes_start;
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'"'
            && bytes
                .get(index + 1..index + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Some(index + 1 + hashes);
        }
        index += 1;
    }
    Some(bytes.len())
}

fn quoted_string_end(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn char_literal_end(source: &str, quote: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let content = quote + 1;
    let first = *bytes.get(content)?;
    let after_content = if first == b'\\' {
        escaped_char_end(bytes, content)?
    } else {
        let character = source.get(content..)?.chars().next()?;
        content + character.len_utf8()
    };
    (bytes.get(after_content) == Some(&b'\'')).then_some(after_content + 1)
}

fn escaped_char_end(bytes: &[u8], slash: usize) -> Option<usize> {
    let escape = *bytes.get(slash + 1)?;
    match escape {
        b'x' => bytes.get(slash + 3).map(|_| slash + 4),
        b'u' if bytes.get(slash + 2) == Some(&b'{') => {
            let mut index = slash + 3;
            while bytes.get(index).is_some_and(|byte| *byte != b'}') {
                index += 1;
            }
            (bytes.get(index) == Some(&b'}')).then_some(index + 1)
        }
        _ => Some(slash + 2),
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
    fn comments_and_literals_containing_client_module_text_are_allowed() {
        for generated in [
            "const S: &str = \"market_service_client {\";\n",
            "const S: &str = r#\"market_service_client {\"#;\n",
            "const S: &[u8] = b\"market_service_client {\";\n",
            "const S: &[u8] = br#\"market_service_client {\"#;\n",
            "const S: &CStr = c\"market_service_client {\";\n",
            "const S: &CStr = cr#\"market_service_client {\"#;\n",
            "// market_service_client {\npub struct Response;\n",
            "/* market_service_client { */ pub struct Response;\n",
            "let market_service_client = { Response::default() };\n",
        ] {
            assert_eq!(grpc_client_module(generated), None, "{generated}");
        }
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
