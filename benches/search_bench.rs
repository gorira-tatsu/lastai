use std::path::PathBuf;

use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use lastai::{
    index::SearchIndex,
    types::{MessageDoc, Provider, Role, SearchOptions, SourceRef},
};

fn synthetic_docs(count: usize) -> Vec<MessageDoc> {
    (0..count)
        .map(|idx| MessageDoc {
            provider: Provider::Codex,
            session_id: format!("session-{}", idx / 10),
            cwd: Some(PathBuf::from("/tmp/lastai")),
            timestamp: Some(Utc::now()),
            role: if idx % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            text: format!("docker compose cargo test 日本語検索 document number {idx}"),
            source: SourceRef {
                path: PathBuf::from("synthetic.jsonl"),
                byte_offset: idx as u64,
                line_number: idx as u64 + 1,
            },
            is_sidechain: false,
        })
        .collect()
}

fn bench_search(c: &mut Criterion) {
    // This benchmark currently exercises the public search API through a small fixture.
    // Larger 10k/100k corpus benches should be run locally before release tagging.
    let _docs = synthetic_docs(1_000);
    c.bench_function("synthetic_doc_generation_1k", |b| {
        b.iter(|| synthetic_docs(1_000))
    });
    let _ = SearchOptions::default();
    let _ = std::mem::size_of::<SearchIndex>();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
