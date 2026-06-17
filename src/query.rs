use chrono::{DateTime, NaiveDate, Utc};

use crate::{
    tokenizer,
    types::{Provider, Role},
};

#[derive(Debug, Clone, Default)]
pub struct ParsedQuery {
    pub clauses: Vec<QueryClause>,
    pub filters: QueryFilters,
}

#[derive(Debug, Clone)]
pub enum QueryClause {
    Term { raw: String, tokens: Vec<String> },
    Prefix { raw: String, prefix: String },
    Phrase { raw: String, tokens: Vec<String> },
}

#[derive(Debug, Clone, Default)]
pub struct QueryFilters {
    pub provider: Option<Provider>,
    pub cwd_substring: Option<String>,
    pub role: Option<Role>,
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub session: Option<String>,
    pub sidechain: Option<bool>,
}

pub fn parse_query(input: &str) -> ParsedQuery {
    let mut query = ParsedQuery::default();
    for part in split_query(input) {
        if apply_filter(&part, &mut query.filters) {
            continue;
        }
        if part.starts_with('"') && part.ends_with('"') && part.len() >= 2 {
            let raw = part[1..part.len() - 1].to_string();
            let tokens = tokenizer::tokenize(&raw)
                .into_iter()
                .map(|token| token.text)
                .collect::<Vec<_>>();
            query.clauses.push(QueryClause::Phrase { raw, tokens });
            continue;
        }
        if let Some(prefix) = part.strip_suffix('*') {
            let normalized = tokenizer::normalize(prefix);
            if !normalized.is_empty() {
                query.clauses.push(QueryClause::Prefix {
                    raw: part,
                    prefix: normalized,
                });
            }
            continue;
        }
        let tokens = tokenizer::tokenize(&part)
            .into_iter()
            .map(|token| token.text)
            .collect::<Vec<_>>();
        if !tokens.is_empty() {
            query.clauses.push(QueryClause::Term { raw: part, tokens });
        }
    }
    query
}

pub fn extract_search_text(input: &str) -> String {
    split_query(input)
        .into_iter()
        .filter(|part| !is_filter(part))
        .map(|part| {
            if part.starts_with('"') && part.ends_with('"') && part.len() >= 2 {
                part[1..part.len() - 1].to_string()
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn split_query(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;

    for ch in input.chars() {
        match ch {
            '"' => {
                if in_quote {
                    if !current.is_empty() {
                        let raw = std::mem::take(&mut current);
                        parts.push(format!("\"{raw}\""));
                    }
                    in_quote = false;
                } else {
                    if !current.trim().is_empty() {
                        parts.push(current.trim().to_string());
                        current.clear();
                    }
                    in_quote = true;
                }
            }
            ch if ch.is_whitespace() && !in_quote => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        if in_quote {
            parts.push(format!("\"{}\"", current.trim()));
        } else {
            parts.push(current.trim().to_string());
        }
    }

    parts.into_iter().collect()
}

fn apply_filter(part: &str, filters: &mut QueryFilters) -> bool {
    let Some((key, value)) = part.split_once(':') else {
        return false;
    };
    if value.is_empty() {
        return false;
    }
    match key {
        "provider" => {
            filters.provider = value.parse().ok();
            filters.provider.is_some()
        }
        "cwd" => {
            filters.cwd_substring = Some(tokenizer::normalize(value));
            true
        }
        "role" => {
            filters.role = Some(Role::from_label(value));
            true
        }
        "after" => {
            filters.after =
                parse_day(value).map(|date| date.and_hms_opt(0, 0, 0).unwrap().and_utc());
            filters.after.is_some()
        }
        "before" => {
            filters.before =
                parse_day(value).map(|date| date.and_hms_opt(23, 59, 59).unwrap().and_utc());
            filters.before.is_some()
        }
        "session" => {
            filters.session = Some(value.to_string());
            true
        }
        "sidechain" => {
            filters.sidechain = match value {
                "true" | "1" | "yes" | "on" => Some(true),
                "false" | "0" | "no" | "off" => Some(false),
                _ => None,
            };
            filters.sidechain.is_some()
        }
        _ => false,
    }
}

fn is_filter(part: &str) -> bool {
    let Some((key, value)) = part.split_once(':') else {
        return false;
    };
    !value.is_empty()
        && matches!(
            key,
            "provider" | "cwd" | "role" | "after" | "before" | "session" | "sidechain"
        )
}

fn parse_day(input: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d").ok()
}

pub fn phrase_from_clause(clause: &QueryClause) -> Option<&str> {
    match clause {
        QueryClause::Phrase { raw, .. } => Some(raw),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filters_and_prefix() {
        let parsed = parse_query("provider:codex role:user docker* after:2026-06-01");
        assert_eq!(parsed.filters.provider, Some(Provider::Codex));
        assert_eq!(parsed.filters.role, Some(Role::User));
        assert!(matches!(parsed.clauses[0], QueryClause::Prefix { .. }));
        assert!(parsed.filters.after.is_some());
    }

    #[test]
    fn parses_phrase() {
        let parsed = parse_query("\"docker compose\" role:user");
        assert!(matches!(parsed.clauses[0], QueryClause::Phrase { .. }));
        assert_eq!(parsed.filters.role, Some(Role::User));
    }

    #[test]
    fn extracts_search_text_without_filters() {
        assert_eq!(
            extract_search_text("provider:codex \"docker compose\" role:user"),
            "docker compose"
        );
    }
}
