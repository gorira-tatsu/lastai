use std::collections::hash_map::DefaultHasher;
use std::{
    collections::HashSet,
    fs::File,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::Value;

use crate::{
    tokenizer,
    types::{MessageDoc, Provider, Role, SourceRef},
};

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub provider: Provider,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub docs: Vec<MessageDoc>,
    pub end_offset: u64,
}

#[derive(Default, Debug, Clone)]
struct ContextState {
    session_id: Option<String>,
    cwd: Option<PathBuf>,
}

pub fn discover_source_files() -> Vec<SourceFile> {
    let mut files = Vec::new();
    files.extend(discover_codex_files());
    files.extend(discover_claude_files());
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

pub fn discover_codex_root() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

pub fn discover_claude_root() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"))
}

pub fn discover_codex_files() -> Vec<SourceFile> {
    let root = discover_codex_root();
    let mut files = Vec::new();
    for dir in [root.join("sessions"), root.join("archived_sessions")] {
        if !dir.exists() {
            continue;
        }
        for entry in WalkBuilder::new(dir).hidden(false).build().flatten() {
            if entry.file_type().is_some_and(|ft| ft.is_file())
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
            {
                files.push(SourceFile {
                    provider: Provider::Codex,
                    path: entry.into_path(),
                });
            }
        }
    }
    files
}

pub fn discover_claude_files() -> Vec<SourceFile> {
    let root = discover_claude_root().join("projects");
    if !root.exists() {
        return Vec::new();
    }
    WalkBuilder::new(root)
        .hidden(false)
        .build()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_some_and(|ft| ft.is_file())
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        })
        .map(|entry| SourceFile {
            provider: Provider::Claude,
            path: entry.into_path(),
        })
        .collect()
}

pub fn parse_source_file(
    file: &SourceFile,
    emit_after_offset: u64,
    max_text_bytes: usize,
) -> Result<ParsedFile> {
    let mut reader = BufReader::new(
        File::open(&file.path)
            .with_context(|| format!("failed to open {}", file.path.display()))?,
    );
    let mut ctx = ContextState::default();
    let mut docs = Vec::new();
    let mut offset = 0u64;
    let mut line_number = 0u64;

    loop {
        let mut bytes = Vec::new();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .with_context(|| format!("failed to read {}", file.path.display()))?;
        if read == 0 {
            break;
        }
        let line_start = offset;
        offset += read as u64;
        line_number += 1;
        while matches!(bytes.last(), Some(b'\r' | b'\n')) {
            bytes.pop();
        }
        if bytes.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        if !should_parse_line(file.provider, &bytes) {
            continue;
        }
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let parsed = match file.provider {
            Provider::Codex => parse_codex_line(
                &value,
                &mut ctx,
                &file.path,
                line_start,
                line_number,
                max_text_bytes,
            ),
            Provider::Claude => parse_claude_line(
                &value,
                &mut ctx,
                &file.path,
                line_start,
                line_number,
                max_text_bytes,
            ),
        };
        if line_start >= emit_after_offset
            && let Some(doc) = parsed
        {
            docs.push(doc);
        }
    }

    Ok(ParsedFile {
        docs: dedupe_message_docs(docs),
        end_offset: offset,
    })
}

pub fn dedupe_message_docs(docs: Vec<MessageDoc>) -> Vec<MessageDoc> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(docs.len());
    for doc in docs {
        if seen.insert(message_identity(&doc)) {
            deduped.push(doc);
        }
    }
    deduped
}

fn should_parse_line(provider: Provider, bytes: &[u8]) -> bool {
    match provider {
        Provider::Codex => {
            contains_bytes(bytes, br#""type":"session_meta""#)
                || contains_bytes(bytes, br#""type":"turn_context""#)
                || contains_bytes(bytes, br#""role":"user""#)
                || contains_bytes(bytes, br#""role":"assistant""#)
                || contains_bytes(bytes, br#""type":"user""#)
                || contains_bytes(bytes, br#""type":"assistant""#)
        }
        Provider::Claude => {
            contains_bytes(bytes, br#""type":"user""#)
                || contains_bytes(bytes, br#""type":"assistant""#)
                || contains_bytes(bytes, br#""role":"user""#)
                || contains_bytes(bytes, br#""role":"assistant""#)
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn parse_codex_line(
    value: &Value,
    ctx: &mut ContextState,
    path: &Path,
    byte_offset: u64,
    line_number: u64,
    max_text_bytes: usize,
) -> Option<MessageDoc> {
    let line_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let payload = value.get("payload").unwrap_or(value);
    if line_type == "session_meta" {
        if let Some(id) = payload.get("id").and_then(Value::as_str) {
            ctx.session_id = Some(id.to_string());
        }
        if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
            ctx.cwd = Some(PathBuf::from(cwd));
        }
        return None;
    }
    if line_type == "turn_context"
        && let Some(cwd) = payload.get("cwd").and_then(Value::as_str)
    {
        ctx.cwd = Some(PathBuf::from(cwd));
    }

    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .map(Role::from_label)
        .unwrap_or_else(|| infer_role(line_type));
    let timestamp = value
        .get("timestamp")
        .or_else(|| payload.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(parse_timestamp);
    let text = codex_text(payload, line_type);
    finish_doc(DocParts {
        provider: Provider::Codex,
        session_id: ctx
            .session_id
            .clone()
            .unwrap_or_else(|| session_id_from_path(path)),
        cwd: ctx.cwd.clone(),
        timestamp,
        role,
        text,
        source: SourceRef {
            path: path.to_path_buf(),
            byte_offset,
            line_number,
        },
        is_sidechain: false,
        max_text_bytes,
    })
}

fn parse_claude_line(
    value: &Value,
    ctx: &mut ContextState,
    path: &Path,
    byte_offset: u64,
    line_number: u64,
    max_text_bytes: usize,
) -> Option<MessageDoc> {
    if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
        ctx.session_id = Some(id.to_string());
    }
    if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
        ctx.cwd = Some(PathBuf::from(cwd));
    }
    let line_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = value.get("message").unwrap_or(value);
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .map(Role::from_label)
        .unwrap_or_else(|| infer_role(line_type));
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_timestamp);
    let text = collect_text(message);
    let is_sidechain = value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || path
            .components()
            .any(|component| component.as_os_str() == "subagents");
    finish_doc(DocParts {
        provider: Provider::Claude,
        session_id: ctx
            .session_id
            .clone()
            .unwrap_or_else(|| session_id_from_path(path)),
        cwd: ctx.cwd.clone(),
        timestamp,
        role,
        text,
        source: SourceRef {
            path: path.to_path_buf(),
            byte_offset,
            line_number,
        },
        is_sidechain,
        max_text_bytes,
    })
}

fn codex_text(payload: &Value, line_type: &str) -> String {
    if let Some(content) = payload.get("content") {
        return collect_text(content);
    }
    if let Some(message) = payload.get("message") {
        return collect_text(message);
    }
    if matches!(line_type, "turn_context" | "event_msg" | "response_item") {
        return collect_text(payload);
    }
    String::new()
}

struct DocParts {
    provider: Provider,
    session_id: String,
    cwd: Option<PathBuf>,
    timestamp: Option<DateTime<Utc>>,
    role: Role,
    text: String,
    source: SourceRef,
    is_sidechain: bool,
    max_text_bytes: usize,
}

fn finish_doc(parts: DocParts) -> Option<MessageDoc> {
    let text = truncate_utf8(parts.text.trim(), parts.max_text_bytes);
    if text.is_empty() {
        return None;
    }
    if should_skip_doc(parts.role, &text) {
        return None;
    }
    Some(MessageDoc {
        provider: parts.provider,
        session_id: parts.session_id,
        cwd: parts.cwd,
        timestamp: parts.timestamp,
        role: parts.role,
        text,
        source: parts.source,
        is_sidechain: parts.is_sidechain,
    })
}

fn collect_text(value: &Value) -> String {
    let mut out = Vec::new();
    collect_text_inner(value, None, &mut out);
    out.join("\n")
}

fn collect_text_inner(value: &Value, key: Option<&str>, out: &mut Vec<String>) {
    match value {
        Value::String(text) if key.is_none_or(is_text_key) && !looks_like_metadata(text) => {
            out.push(text.to_string());
        }
        Value::Array(values) => {
            for value in values {
                collect_text_inner(value, key, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if is_metadata_key(key) {
                    continue;
                }
                collect_text_inner(value, Some(key), out);
            }
        }
        _ => {}
    }
}

fn is_text_key(key: &str) -> bool {
    matches!(
        key,
        "text"
            | "content"
            | "message"
            | "summary"
            | "cmd"
            | "command"
            | "output"
            | "stdout"
            | "stderr"
            | "result"
            | "reasoning"
    )
}

fn is_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "uuid"
            | "sessionId"
            | "parentUuid"
            | "requestId"
            | "promptId"
            | "version"
            | "cwd"
            | "timestamp"
            | "type"
            | "role"
            | "userType"
            | "gitBranch"
            | "isSidechain"
    )
}

fn looks_like_metadata(text: &str) -> bool {
    text.len() > 64 && text.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

fn should_skip_doc(role: Role, text: &str) -> bool {
    role == Role::System || is_injected_instruction(text)
}

fn is_injected_instruction(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("# AGENTS.md instructions for ")
        || text.starts_with("# AGENTS.md instructions")
        || text.starts_with("<environment_context>")
        || text.starts_with("<skill>\n<name>")
        || text.starts_with("<skill>\r\n<name>")
        || (text.starts_with("<INSTRUCTIONS>") && text.contains("</INSTRUCTIONS>"))
        || (text.contains("<permissions instructions>") && text.contains("<collaboration_mode>"))
        || (text.contains("### Protect Validation Integrity")
            && text.contains("## Skill Creation Process")
            && text.contains("</skill>"))
}

fn message_identity(doc: &MessageDoc) -> (Provider, String, Role, u64) {
    (
        doc.provider,
        doc.session_id.clone(),
        doc.role,
        stable_hash(doc.text.trim()),
    )
}

fn stable_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn infer_role(label: &str) -> Role {
    match label {
        "user" | "last-prompt" => Role::User,
        "assistant" => Role::Assistant,
        "tool" | "tool_result" | "attachment" => Role::Tool,
        "system" | "session_meta" | "turn_context" => Role::System,
        _ => Role::Other,
    }
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown-session")
        .trim_start_matches("rollout-")
        .to_string()
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = input[..end].to_string();
    truncated.push_str("\n[truncated]");
    truncated
}

pub fn normalized_contains(haystack: &str, needle: &str) -> bool {
    tokenizer::normalize(haystack).contains(&tokenizer::normalize(needle))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn parses_codex_fixture_and_skips_corrupt_lines() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","timestamp":"2026-06-17T00:00:00Z","payload":{{"id":"sid","cwd":"/tmp/project"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","timestamp":"2026-06-17T00:00:01Z","payload":{{"role":"user","content":"hello Codex"}}}}"#
        )
        .unwrap();
        writeln!(file, "{{not json").unwrap();
        let source = SourceFile {
            provider: Provider::Codex,
            path: file.path().to_path_buf(),
        };
        let parsed = parse_source_file(&source, 0, 256 * 1024).unwrap();
        assert_eq!(parsed.docs.len(), 1);
        assert_eq!(parsed.docs[0].session_id, "sid");
        assert_eq!(parsed.docs[0].role, Role::User);
        assert!(parsed.docs[0].text.contains("hello Codex"));
    }

    #[test]
    fn skips_injected_codex_instructions() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"sid","cwd":"/tmp/project"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-06-17T00:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nnoise\n</INSTRUCTIONS>"},
                        {"type": "input_text", "text": "<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"}
                    ]
                }
            })
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","timestamp":"2026-06-17T00:00:01Z","payload":{{"role":"user","content":"real request"}}}}"#
        )
        .unwrap();
        let source = SourceFile {
            provider: Provider::Codex,
            path: file.path().to_path_buf(),
        };
        let parsed = parse_source_file(&source, 0, 256 * 1024).unwrap();
        assert_eq!(parsed.docs.len(), 1);
        assert_eq!(parsed.docs[0].text, "real request");
    }

    #[test]
    fn dedupes_identical_docs_per_session() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"session_meta","payload":{{"id":"sid","cwd":"/tmp/project"}}}}"#
        )
        .unwrap();
        for line in 0..2 {
            writeln!(
                file,
                r#"{{"type":"response_item","timestamp":"2026-06-17T00:00:0{}Z","payload":{{"role":"user","content":"same text"}}}}"#,
                line
            )
            .unwrap();
        }
        let source = SourceFile {
            provider: Provider::Codex,
            path: file.path().to_path_buf(),
        };
        let parsed = parse_source_file(&source, 0, 256 * 1024).unwrap();
        assert_eq!(parsed.docs.len(), 1);
        assert_eq!(parsed.docs[0].text, "same text");
    }

    #[test]
    fn parses_claude_sidechain_fixture() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"user","sessionId":"abc","cwd":"/tmp/project","isSidechain":true,"timestamp":"2026-06-17T00:00:00Z","message":{{"role":"user","content":"日本語検索"}}}}"#
        )
        .unwrap();
        let source = SourceFile {
            provider: Provider::Claude,
            path: file.path().to_path_buf(),
        };
        let parsed = parse_source_file(&source, 0, 256 * 1024).unwrap();
        assert_eq!(parsed.docs.len(), 1);
        assert!(parsed.docs[0].is_sidechain);
        assert_eq!(parsed.docs[0].role, Role::User);
    }

    #[test]
    fn tracks_exact_offset_without_trailing_newline() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"{{"type":"response_item","payload":{{"role":"user","content":"no newline"}}}}"#
        )
        .unwrap();
        let source = SourceFile {
            provider: Provider::Codex,
            path: file.path().to_path_buf(),
        };
        let parsed = parse_source_file(&source, 0, 256 * 1024).unwrap();
        let len = std::fs::metadata(file.path()).unwrap().len();
        assert_eq!(parsed.end_offset, len);
        assert_eq!(parsed.docs.len(), 1);
    }
}
