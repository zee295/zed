use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::fs_rpc::FsRpc;

const KEYWORDS: &[&str] = &[
    "abstract",
    "and",
    "as",
    "async",
    "await",
    "become",
    "box",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "crate",
    "def",
    "defer",
    "delete",
    "do",
    "dyn",
    "else",
    "enum",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "fn",
    "for",
    "from",
    "function",
    "go",
    "if",
    "impl",
    "import",
    "in",
    "interface",
    "is",
    "let",
    "loop",
    "macro",
    "match",
    "mod",
    "move",
    "mut",
    "namespace",
    "new",
    "nil",
    "none",
    "not",
    "null",
    "of",
    "or",
    "override",
    "package",
    "pass",
    "private",
    "protected",
    "pub",
    "public",
    "raise",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "switch",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "union",
    "unsafe",
    "use",
    "using",
    "var",
    "virtual",
    "void",
    "where",
    "while",
    "with",
    "yield",
];

const TYPE_KEYWORDS: &[&str] = &[
    "any", "bool", "boolean", "byte", "char", "double", "f32", "f64", "float", "i8", "i16", "i32",
    "i64", "int", "isize", "long", "never", "number", "object", "short", "str", "string", "u8",
    "u16", "u32", "u64", "uint", "usize",
];

pub fn dispatch(fs: &FsRpc, params: &Value) -> Result<Value> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = match params.get("text").and_then(Value::as_str) {
        Some(text) => text.to_string(),
        None => {
            let file = fs.path(path)?;
            if !file.is_file() {
                return Ok(json!({
                    "spans": [],
                    "language": Value::Null,
                    "error": "not a file"
                }));
            }
            String::from_utf8_lossy(&std::fs::read(file)?).into_owned()
        }
    };
    let language = params
        .get("language")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| language_from_path(path).map(ToOwned::to_owned));
    Ok(json!({
        "spans": tokenize(&text, language.as_deref()),
        "language": language,
        "error": Value::Null,
    }))
}

fn language_from_path(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        "css" => Some("css"),
        "go" => Some("go"),
        "html" | "htm" => Some("html"),
        "java" => Some("java"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "json" | "jsonc" => Some("json"),
        "md" | "mdx" => Some("markdown"),
        "py" | "pyi" => Some("python"),
        "rb" => Some("ruby"),
        "rs" => Some("rust"),
        "sh" | "bash" | "zsh" => Some("shell"),
        "sql" => Some("sql"),
        "swift" => Some("swift"),
        "toml" => Some("toml"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "xml" => Some("xml"),
        "yaml" | "yml" => Some("yaml"),
        _ => None,
    }
}

fn tokenize(text: &str, language: Option<&str>) -> Vec<Value> {
    let bytes = text.as_bytes();
    let hash_comments = matches!(
        language,
        Some("python" | "ruby" | "shell" | "yaml" | "toml")
    );
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") || (hash_comments && bytes[index] == b'#') {
            index = text[index..]
                .find('\n')
                .map(|offset| index + offset)
                .unwrap_or(bytes.len());
            push(&mut spans, start, index, "comment");
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index = text[index + 2..]
                .find("*/")
                .map(|offset| index + 2 + offset + 2)
                .unwrap_or(bytes.len());
            push(&mut spans, start, index, "comment");
            continue;
        }
        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            let quote = bytes[index];
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else if bytes[index] == quote {
                    index += 1;
                    break;
                } else {
                    index += utf8_width(bytes[index]);
                }
            }
            push(&mut spans, start, index, "string");
            continue;
        }
        if bytes[index].is_ascii_digit() {
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || matches!(bytes[index], b'.' | b'_' | b'+' | b'-'))
            {
                index += 1;
            }
            push(&mut spans, start, index, "number");
            continue;
        }
        if identifier_start(bytes[index]) {
            index += utf8_width(bytes[index]);
            while index < bytes.len() && identifier_continue(bytes[index]) {
                index += utf8_width(bytes[index]);
            }
            let word = &text[start..index];
            let lower = word.to_ascii_lowercase();
            let kind = if TYPE_KEYWORDS.binary_search(&lower.as_str()).is_ok() {
                "type"
            } else if KEYWORDS.contains(&lower.as_str()) {
                if matches!(lower.as_str(), "true" | "false" | "null" | "nil" | "none") {
                    "constant"
                } else {
                    "keyword"
                }
            } else if bytes[index..]
                .iter()
                .skip_while(|byte| byte.is_ascii_whitespace())
                .next()
                == Some(&b'(')
            {
                "function"
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                "type"
            } else {
                index = index.max(start + 1);
                continue;
            };
            push(&mut spans, start, index, kind);
            continue;
        }
        index += utf8_width(bytes[index]);
        let kind = if b"{}[]();,.:".contains(&bytes[start]) {
            "punctuation"
        } else {
            "operator"
        };
        push(&mut spans, start, index, kind);
    }
    spans
}

fn identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80
}

fn identifier_continue(byte: u8) -> bool {
    identifier_start(byte) || byte.is_ascii_digit()
}

fn utf8_width(byte: u8) -> usize {
    match byte {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn push(spans: &mut Vec<Value>, start: usize, end: usize, kind: &str) {
    spans.push(json!({"start": start, "end": end, "kind": kind}));
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn highlights_rust_with_utf8_byte_offsets() {
        let text = "fn café() { // ok\n  \"yes\"\n}";
        let spans = tokenize(text, Some("rust"));
        assert!(spans.iter().any(|span| span["kind"] == "keyword"));
        assert!(spans.iter().any(|span| span["kind"] == "function"));
        assert!(spans.iter().any(|span| span["kind"] == "comment"));
        assert!(spans.iter().any(|span| span["kind"] == "string"));
        assert!(
            spans
                .iter()
                .all(|span| span["end"].as_u64().unwrap() <= text.len() as u64)
        );
    }
}
