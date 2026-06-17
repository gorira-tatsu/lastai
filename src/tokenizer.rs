use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub position: u32,
}

pub fn normalize(input: &str) -> String {
    input.nfkc().collect::<String>().to_lowercase()
}

pub fn tokenize(input: &str) -> Vec<Token> {
    let normalized = normalize(input);
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut cjk = String::new();
    let mut position = 0u32;

    for ch in normalized.chars() {
        if ch.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk, &mut tokens, &mut position);
            ascii.push(ch);
        } else if is_cjk(ch) {
            flush_ascii(&mut ascii, &mut tokens, &mut position);
            cjk.push(ch);
        } else {
            flush_ascii(&mut ascii, &mut tokens, &mut position);
            flush_cjk(&mut cjk, &mut tokens, &mut position);
        }
    }
    flush_ascii(&mut ascii, &mut tokens, &mut position);
    flush_cjk(&mut cjk, &mut tokens, &mut position);
    tokens
}

pub fn ngrams_for_text(input: &str) -> Vec<String> {
    let normalized = normalize(input);
    let chars: Vec<char> = normalized.chars().filter(|c| !c.is_whitespace()).collect();
    let mut grams = Vec::new();
    if chars.len() < 3 {
        if !chars.is_empty() {
            grams.push(chars.iter().collect());
        }
        return grams;
    }
    for window in chars.windows(3) {
        grams.push(window.iter().collect());
    }
    let mut cjk_run = Vec::new();
    for ch in chars {
        if is_cjk(ch) {
            cjk_run.push(ch);
        } else {
            push_cjk_bigrams(&cjk_run, &mut grams);
            cjk_run.clear();
        }
    }
    push_cjk_bigrams(&cjk_run, &mut grams);
    grams.sort();
    grams.dedup();
    grams
}

pub fn contains_cjk(input: &str) -> bool {
    input.chars().any(is_cjk)
}

fn flush_ascii(buf: &mut String, tokens: &mut Vec<Token>, position: &mut u32) {
    if buf.is_empty() {
        return;
    }
    let parts = split_identifier(buf);
    if parts.len() > 1 {
        push_token(buf.clone(), tokens, position);
    }
    for part in parts {
        push_token(part, tokens, position);
    }
    buf.clear();
}

fn flush_cjk(buf: &mut String, tokens: &mut Vec<Token>, position: &mut u32) {
    if buf.is_empty() {
        return;
    }
    let chars: Vec<char> = buf.chars().collect();
    if chars.len() == 1 {
        push_token(chars[0].to_string(), tokens, position);
    } else {
        for window in chars.windows(2) {
            push_token(window.iter().collect(), tokens, position);
        }
        for window in chars.windows(3) {
            push_token(window.iter().collect(), tokens, position);
        }
    }
    buf.clear();
}

fn split_identifier(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = input.chars().collect();
    for (idx, ch) in chars.iter().enumerate() {
        if idx > 0 {
            let prev = chars[idx - 1];
            if ch.is_ascii_uppercase()
                && (prev.is_ascii_lowercase() || prev.is_ascii_digit())
                && !current.is_empty()
            {
                parts.push(current.to_lowercase());
                current.clear();
            }
        }
        current.push(*ch);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts
}

fn push_token(text: String, tokens: &mut Vec<Token>, position: &mut u32) {
    if text.is_empty() {
        return;
    }
    tokens.push(Token {
        text,
        position: *position,
    });
    *position += 1;
}

fn push_cjk_bigrams(chars: &[char], grams: &mut Vec<String>) {
    if chars.len() >= 2 {
        for window in chars.windows(2) {
            grams.push(window.iter().collect());
        }
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3040..=0x30ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xf900..=0xfaff
            | 0xac00..=0xd7af
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_mixed_text() {
        let terms: Vec<_> = tokenize("foo_bar CamelCase /tmp/my-file 日本語")
            .into_iter()
            .map(|t| t.text)
            .collect();
        assert!(terms.contains(&"foo".to_string()));
        assert!(terms.contains(&"bar".to_string()));
        assert!(terms.contains(&"camelcase".to_string()));
        assert!(terms.contains(&"tmp".to_string()));
        assert!(terms.contains(&"日本".to_string()));
        assert!(terms.contains(&"日本語".to_string()));
    }

    #[test]
    fn creates_cjk_ngrams() {
        let grams = ngrams_for_text("日本語検索");
        assert!(grams.contains(&"日本".to_string()));
        assert!(grams.contains(&"日本語".to_string()));
    }
}
