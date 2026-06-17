use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use regex::RegexBuilder;

use crate::{
    config::AppConfig,
    providers::{dedupe_message_docs, discover_source_files, parse_source_file},
    query::{QueryFilters, extract_search_text, parse_query},
    tokenizer,
    types::{MessageDoc, Provider, SearchOptions, SessionHit, Snippet},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ScanMode {
    Literal,
    Regex,
    Fuzzy,
}

pub fn scan_search(
    config: &AppConfig,
    query: &str,
    mode: ScanMode,
    options: SearchOptions,
) -> Result<Vec<SessionHit>> {
    let docs = load_scan_docs(config)?;
    search_docs(&docs, query, mode, options)
}

pub fn load_scan_docs(config: &AppConfig) -> Result<Vec<MessageDoc>> {
    let files = discover_source_files();
    let docs = files
        .par_iter()
        .map(|file| {
            parse_source_file(file, 0, config.max_indexed_bytes_per_message)
                .map(|parsed| parsed.docs)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    Ok(dedupe_message_docs(docs))
}

pub fn search_docs(
    docs: &[MessageDoc],
    query: &str,
    mode: ScanMode,
    mut options: SearchOptions,
) -> Result<Vec<SessionHit>> {
    if options.limit == 0 {
        options.limit = 50;
    }
    let parsed = parse_query(query);
    let search_text = extract_search_text(query);
    let matcher = Matcher::new(&search_text, mode)?;
    let mut by_session: HashMap<(Provider, String), SessionAccumulator> = HashMap::new();

    for doc in docs {
        if !matches_filters(doc, &parsed.filters, options.default_sidechain) {
            continue;
        }
        let Some(mut score) = matcher.score(&doc.text) else {
            continue;
        };
        if let (Some(current), Some(cwd)) = (options.current_cwd.as_ref(), doc.cwd.as_ref()) {
            if current == cwd {
                score += 2.0;
            } else if cwd.starts_with(current) || current.starts_with(cwd) {
                score += 1.0;
            }
        }
        if let Some(timestamp) = doc.timestamp {
            let age_days = (Utc::now() - timestamp).num_days().max(0) as f32;
            score += 1.0 / (1.0 + age_days / 30.0);
        }
        let entry = by_session
            .entry((doc.provider, doc.session_id.clone()))
            .or_insert_with(|| SessionAccumulator::new(doc));
        entry.add(doc, score, snippet_for(doc, &search_text));
    }

    let mut hits = by_session
        .into_values()
        .map(SessionAccumulator::finish)
        .collect::<Vec<_>>();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
    });
    hits.truncate(options.limit);
    Ok(hits)
}

enum Matcher {
    Empty,
    Literal(Vec<String>),
    Regex(regex::Regex),
    Fuzzy(String),
}

impl Matcher {
    fn new(query: &str, mode: ScanMode) -> Result<Self> {
        if query.trim().is_empty() {
            return Ok(Self::Empty);
        }
        Ok(match mode {
            ScanMode::Literal => Self::Literal(
                tokenizer::tokenize(query)
                    .into_iter()
                    .map(|token| token.text)
                    .collect(),
            ),
            ScanMode::Regex => Self::Regex(
                RegexBuilder::new(query)
                    .case_insensitive(true)
                    .build()
                    .with_context(|| format!("invalid regex: {query}"))?,
            ),
            ScanMode::Fuzzy => Self::Fuzzy(tokenizer::normalize(query)),
        })
    }

    fn score(&self, text: &str) -> Option<f32> {
        match self {
            Self::Empty => Some(1.0),
            Self::Literal(tokens) => {
                let normalized = tokenizer::normalize(text);
                let mut score = 0.0;
                for token in tokens {
                    if !normalized.contains(token) {
                        return None;
                    }
                    score += 2.0;
                }
                Some(score + 1.0)
            }
            Self::Regex(regex) => regex.find(text).map(|m| {
                let early = 1.0 / (1.0 + m.start() as f32 / 80.0);
                4.0 + early + (m.end() - m.start()).min(80) as f32 / 80.0
            }),
            Self::Fuzzy(pattern) => fuzzy_score(&tokenizer::normalize(text), pattern),
        }
    }
}

fn fuzzy_score(text: &str, pattern: &str) -> Option<f32> {
    if pattern.is_empty() {
        return Some(1.0);
    }
    let mut score = 0.0;
    let mut last_match: Option<usize> = None;
    let mut start_match: Option<usize> = None;
    let mut cursor = 0usize;
    let chars = text.char_indices().collect::<Vec<_>>();

    for needle in pattern.chars().filter(|ch| !ch.is_whitespace()) {
        let mut found = None;
        for (idx, (byte_idx, ch)) in chars.iter().enumerate().skip(cursor) {
            if *ch == needle {
                found = Some((idx, *byte_idx));
                break;
            }
        }
        let (idx, byte_idx) = found?;
        if start_match.is_none() {
            start_match = Some(byte_idx);
        }
        score += 1.0;
        if last_match.is_some_and(|last| last + 1 == idx) {
            score += 3.0;
        }
        if byte_idx == 0
            || text[..byte_idx]
                .chars()
                .last()
                .is_some_and(|ch| matches!(ch, '/' | '-' | '_' | ' ' | ':' | '.'))
        {
            score += 1.5;
        }
        last_match = Some(idx);
        cursor = idx + 1;
    }

    let early_bonus = 2.0 / (1.0 + start_match.unwrap_or(0) as f32 / 40.0);
    Some(score + early_bonus)
}

fn matches_filters(
    doc: &MessageDoc,
    filters: &QueryFilters,
    default_sidechain: Option<bool>,
) -> bool {
    if filters
        .provider
        .is_some_and(|provider| provider != doc.provider)
    {
        return false;
    }
    if filters.role.is_some_and(|role| role != doc.role) {
        return false;
    }
    let sidechain = filters.sidechain.or(default_sidechain);
    if sidechain.is_some_and(|sidechain| sidechain != doc.is_sidechain) {
        return false;
    }
    if let Some(session) = &filters.session
        && !doc.session_id.contains(session)
    {
        return false;
    }
    if let Some(cwd_filter) = &filters.cwd_substring {
        let cwd = doc
            .cwd
            .as_ref()
            .map(|cwd| tokenizer::normalize(&cwd.display().to_string()))
            .unwrap_or_default();
        if !cwd.contains(cwd_filter) {
            return false;
        }
    }
    if let Some(after) = filters.after
        && doc.timestamp.is_none_or(|timestamp| timestamp < after)
    {
        return false;
    }
    if let Some(before) = filters.before
        && doc.timestamp.is_none_or(|timestamp| timestamp > before)
    {
        return false;
    }
    true
}

fn snippet_for(doc: &MessageDoc, query: &str) -> Snippet {
    let text = if query.is_empty() {
        doc.text.chars().take(240).collect()
    } else {
        snippet_text(&doc.text, query)
    };
    Snippet {
        role: doc.role,
        timestamp: doc.timestamp,
        text,
        source: doc.source.clone(),
    }
}

fn snippet_text(text: &str, needle: &str) -> String {
    if text.len() <= 240 {
        return text.to_string();
    }
    let normalized_text = tokenizer::normalize(text);
    let normalized_needle = tokenizer::normalize(needle);
    let start = normalized_text
        .find(&normalized_needle)
        .map(|idx| idx.saturating_sub(80))
        .unwrap_or(0);
    let start = floor_char_boundary(text, start.min(text.len()));
    text[start..].chars().take(240).collect()
}

fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

struct SessionAccumulator {
    provider: Provider,
    session_id: String,
    cwd: Option<PathBuf>,
    timestamp: Option<DateTime<Utc>>,
    score: f32,
    snippets: Vec<(f32, Snippet)>,
}

impl SessionAccumulator {
    fn new(doc: &MessageDoc) -> Self {
        Self {
            provider: doc.provider,
            session_id: doc.session_id.clone(),
            cwd: doc.cwd.clone(),
            timestamp: doc.timestamp,
            score: 0.0,
            snippets: Vec::new(),
        }
    }

    fn add(&mut self, doc: &MessageDoc, score: f32, snippet: Snippet) {
        self.score = self.score.max(score) + score * 0.08;
        if doc.timestamp > self.timestamp {
            self.timestamp = doc.timestamp;
        }
        if self.cwd.is_none() {
            self.cwd = doc.cwd.clone();
        }
        self.snippets.push((score, snippet));
    }

    fn finish(mut self) -> SessionHit {
        self.snippets
            .sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen_snippets = HashSet::new();
        SessionHit {
            provider: self.provider,
            session_id: self.session_id,
            cwd: self.cwd,
            timestamp: self.timestamp,
            score: self.score,
            snippets: self
                .snippets
                .into_iter()
                .filter_map(|(_, snippet)| {
                    seen_snippets
                        .insert(snippet_key(&snippet))
                        .then_some(snippet)
                })
                .take(3)
                .collect(),
        }
    }
}

fn snippet_key(snippet: &Snippet) -> String {
    format!("{}:{}", snippet.role, tokenizer::normalize(&snippet.text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Role, SourceRef};

    fn doc(text: &str) -> MessageDoc {
        MessageDoc {
            provider: Provider::Codex,
            session_id: "session".to_string(),
            cwd: None,
            timestamp: Some(Utc::now()),
            role: Role::User,
            text: text.to_string(),
            source: SourceRef {
                path: PathBuf::from("fixture.jsonl"),
                byte_offset: 0,
                line_number: 1,
            },
            is_sidechain: false,
        }
    }

    #[test]
    fn fuzzy_matches_ordered_characters() {
        assert!(fuzzy_score("docker compose", "dcp").is_some());
        assert!(fuzzy_score("docker compose", "zzz").is_none());
    }

    #[test]
    fn regex_scan_matches_docs() {
        let hits = search_docs(
            &[doc("cargo test failed")],
            "cargo.*failed",
            ScanMode::Regex,
            SearchOptions::default(),
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
    }
}
