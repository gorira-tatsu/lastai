use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use fst::SetBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    paths::AppPaths,
    providers::{SourceFile, dedupe_message_docs, discover_source_files, parse_source_file},
    query::{ParsedQuery, QueryClause, QueryFilters, parse_query},
    tokenizer,
    types::{MessageDoc, Provider, SearchOptions, SessionHit, Snippet},
    varint,
};

const MANIFEST_VERSION: u32 = 1;
const SNIPPETS_PER_SESSION: usize = 3;
const MAX_TOKEN_INDEX_BYTES_PER_DOC: usize = 64 * 1024;
const MAX_NGRAM_INDEX_BYTES_PER_DOC: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexManifest {
    pub version: u32,
    pub max_indexed_bytes_per_message: usize,
    pub segments: Vec<SegmentMeta>,
    pub tracked_files: BTreeMap<String, FileState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub id: String,
    pub file_name: String,
    pub docs: usize,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub provider: Provider,
    pub size: u64,
    pub modified_millis: i64,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentDisk {
    id: String,
    created_at: DateTime<Utc>,
    docs: Vec<MessageDoc>,
    term_postings: BTreeMap<String, Vec<u8>>,
    ngram_postings: BTreeMap<String, Vec<u8>>,
    fst_terms: Vec<u8>,
}

#[derive(Debug, Clone)]
struct SegmentIndex {
    docs: Vec<MessageDoc>,
    term_postings: BTreeMap<String, Vec<u8>>,
    ngram_postings: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Posting {
    doc_id: u32,
    term_freq: u32,
    positions: Vec<u32>,
}

#[derive(Default)]
struct PostingBuilder {
    positions: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct IndexStats {
    pub segments: usize,
    pub docs: usize,
    pub tracked_files: usize,
    pub index_dir: PathBuf,
}

pub struct IndexManager {
    paths: AppPaths,
    config: AppConfig,
}

pub struct SearchIndex {
    manifest: IndexManifest,
    segments: Vec<SegmentIndex>,
}

#[derive(Default)]
struct PostingCache {
    term: HashMap<String, Vec<Posting>>,
    ngram: HashMap<String, Vec<Posting>>,
}

impl Default for IndexManifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            max_indexed_bytes_per_message: 256 * 1024,
            segments: Vec::new(),
            tracked_files: BTreeMap::new(),
        }
    }
}

impl IndexManager {
    pub fn new(paths: AppPaths, config: AppConfig) -> Self {
        Self { paths, config }
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn load_search_index(&self) -> Result<SearchIndex> {
        SearchIndex::load(&self.paths)
    }

    pub fn update(&self) -> Result<IndexStats> {
        fs::create_dir_all(&self.paths.segments_dir)?;
        let mut manifest = self.load_manifest()?.unwrap_or_default();
        manifest.version = MANIFEST_VERSION;
        manifest.max_indexed_bytes_per_message = self.config.max_indexed_bytes_per_message;

        let files = self.scoped_source_files();
        let mut all_docs = Vec::new();
        for file in files {
            let key = path_key(&file.path);
            let metadata = match fs::metadata(&file.path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let state = manifest.tracked_files.get(&key);
            if state.is_some_and(|state| metadata.len() < state.offset) {
                bail!(
                    "{} shrank since the last index update; run `lastai index rebuild`",
                    file.path.display()
                );
            }
            let emit_after = state.map(|state| state.offset).unwrap_or(0);
            if metadata.len() == emit_after {
                continue;
            }
            let parsed =
                parse_source_file(&file, emit_after, self.config.max_indexed_bytes_per_message)?;
            manifest.tracked_files.insert(
                key,
                FileState {
                    provider: file.provider,
                    size: metadata.len(),
                    modified_millis: modified_millis(&metadata),
                    offset: parsed.end_offset,
                },
            );
            all_docs.extend(parsed.docs);
        }

        if !all_docs.is_empty() {
            let segment = build_segment(
                dedupe_message_docs(all_docs),
                self.config.max_indexed_bytes_per_message,
            )?;
            let meta = save_segment(&self.paths, &segment)?;
            manifest.segments.push(meta);
        }
        self.save_manifest(&manifest)?;
        self.stats_from_manifest(&manifest)
    }

    pub fn rebuild(&self) -> Result<IndexStats> {
        let temp_paths = self.temp_index_paths("rebuild");
        if temp_paths.index_dir.exists() {
            fs::remove_dir_all(&temp_paths.index_dir)?;
        }
        fs::create_dir_all(&temp_paths.segments_dir)?;
        let mut manifest = IndexManifest {
            max_indexed_bytes_per_message: self.config.max_indexed_bytes_per_message,
            ..IndexManifest::default()
        };
        let files = self.scoped_source_files();
        let parsed_files = files
            .par_iter()
            .map(|file| {
                let parsed = parse_source_file(file, 0, self.config.max_indexed_bytes_per_message)?;
                Ok::<(SourceFile, crate::providers::ParsedFile), anyhow::Error>((
                    file.clone(),
                    parsed,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut docs = Vec::new();
        for (file, parsed) in parsed_files {
            if let Ok(metadata) = fs::metadata(&file.path) {
                manifest.tracked_files.insert(
                    path_key(&file.path),
                    FileState {
                        provider: file.provider,
                        size: metadata.len(),
                        modified_millis: modified_millis(&metadata),
                        offset: parsed.end_offset,
                    },
                );
            }
            docs.extend(parsed.docs);
        }
        if !files.is_empty() && docs.is_empty() {
            bail!(
                "rebuild parsed 0 docs from {} source files; keeping existing index",
                files.len()
            );
        }
        if !docs.is_empty() {
            let segment = build_segment(
                dedupe_message_docs(docs),
                self.config.max_indexed_bytes_per_message,
            )?;
            manifest.segments.push(save_segment(&temp_paths, &segment)?);
        }
        save_manifest_to(&temp_paths, &manifest)?;
        replace_index_dir(&self.paths, &temp_paths)?;
        self.stats_from_manifest(&manifest)
    }

    pub fn compact(&self) -> Result<IndexStats> {
        let loaded = self.load_search_index()?;
        let mut seen = BTreeSet::new();
        let mut docs = Vec::new();
        for segment in loaded.segments {
            for doc in segment.docs {
                let key = (
                    doc.provider,
                    doc.source.path.clone(),
                    doc.source.line_number,
                    doc.source.byte_offset,
                );
                if seen.insert(key) {
                    docs.push(doc);
                }
            }
        }
        if self.paths.segments_dir.exists() {
            fs::remove_dir_all(&self.paths.segments_dir)?;
        }
        fs::create_dir_all(&self.paths.segments_dir)?;
        let mut manifest = loaded.manifest;
        manifest.segments.clear();
        if !docs.is_empty() {
            let segment = build_segment(
                dedupe_message_docs(docs),
                self.config.max_indexed_bytes_per_message,
            )?;
            manifest.segments.push(save_segment(&self.paths, &segment)?);
        }
        self.save_manifest(&manifest)?;
        self.stats_from_manifest(&manifest)
    }

    pub fn status(&self) -> Result<IndexStats> {
        let manifest = self.load_manifest()?.unwrap_or_default();
        self.stats_from_manifest(&manifest)
    }

    fn load_manifest(&self) -> Result<Option<IndexManifest>> {
        if !self.paths.manifest_file.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&self.paths.manifest_file)
            .with_context(|| format!("failed to read {}", self.paths.manifest_file.display()))?;
        let manifest = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", self.paths.manifest_file.display()))?;
        Ok(Some(manifest))
    }

    fn save_manifest(&self, manifest: &IndexManifest) -> Result<()> {
        save_manifest_to(&self.paths, manifest)
    }

    fn stats_from_manifest(&self, manifest: &IndexManifest) -> Result<IndexStats> {
        Ok(IndexStats {
            segments: manifest.segments.len(),
            docs: manifest.segments.iter().map(|segment| segment.docs).sum(),
            tracked_files: manifest.tracked_files.len(),
            index_dir: self.paths.index_dir.clone(),
        })
    }

    fn scoped_source_files(&self) -> Vec<SourceFile> {
        let now = Utc::now();
        let cutoff = self
            .config
            .index_recent_days
            .map(|days| now - Duration::days(days.max(0)));
        let mut files = discover_source_files()
            .into_iter()
            .filter_map(|file| {
                let metadata = fs::metadata(&file.path).ok()?;
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| DateTime::<Utc>::from(UNIX_EPOCH + duration))
                    .unwrap_or_else(|| DateTime::<Utc>::from(UNIX_EPOCH));
                if cutoff.is_some_and(|cutoff| modified < cutoff) {
                    return None;
                }
                Some((file, modified))
            })
            .collect::<Vec<_>>();
        files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.path.cmp(&b.0.path)));
        let max_files = self.config.index_max_files;
        if max_files > 0 {
            files.truncate(max_files);
        }
        files.into_iter().map(|(file, _)| file).collect()
    }

    fn temp_index_paths(&self, operation: &str) -> AppPaths {
        let index_dir = self
            .paths
            .cache_dir
            .join(format!("index-{operation}-{}", new_segment_id()));
        paths_for_index_dir(&self.paths, index_dir)
    }
}

impl SearchIndex {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        if !paths.manifest_file.exists() {
            return Ok(Self {
                manifest: IndexManifest::default(),
                segments: Vec::new(),
            });
        }
        let raw = fs::read_to_string(&paths.manifest_file)
            .with_context(|| format!("failed to read {}", paths.manifest_file.display()))?;
        let manifest: IndexManifest = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", paths.manifest_file.display()))?;
        let mut segments = Vec::new();
        for meta in &manifest.segments {
            let path = paths.segments_dir.join(&meta.file_name);
            if !path.exists() {
                continue;
            }
            segments.push(load_segment(meta.clone(), &path)?);
        }
        Ok(Self { manifest, segments })
    }

    pub fn is_empty(&self) -> bool {
        self.segments.iter().all(|segment| segment.docs.is_empty())
    }

    pub fn search(&self, input: &str, mut options: SearchOptions) -> Vec<SessionHit> {
        if options.limit == 0 {
            options.limit = 50;
        }
        let mut parsed = parse_query(input);
        if parsed.filters.sidechain.is_none() {
            parsed.filters.sidechain = options.default_sidechain;
        }
        let mut by_session: HashMap<(Provider, String), SessionAccumulator> = HashMap::new();
        for segment in &self.segments {
            let mut cache = PostingCache::default();
            let candidates = segment_candidates(segment, &parsed, &mut cache);
            for doc_id in candidates {
                let Some(doc) = segment.docs.get(doc_id as usize) else {
                    continue;
                };
                if !matches_filters(doc, &parsed.filters) {
                    continue;
                }
                let score = score_doc(
                    segment,
                    &mut cache,
                    doc_id,
                    doc,
                    &parsed,
                    options.current_cwd.as_ref(),
                );
                let entry = by_session
                    .entry((doc.provider, doc.session_id.clone()))
                    .or_insert_with(|| SessionAccumulator::new(doc));
                entry.add(doc, score, make_snippet(doc, &parsed));
            }
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
        hits
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats {
            segments: self.segments.len(),
            docs: self.segments.iter().map(|segment| segment.docs.len()).sum(),
            tracked_files: self.manifest.tracked_files.len(),
            index_dir: PathBuf::new(),
        }
    }

    pub fn recent_sessions(
        &self,
        limit: usize,
        default_sidechain: Option<bool>,
    ) -> Vec<SessionHit> {
        let mut by_session: HashMap<(Provider, String), RecentAccumulator> = HashMap::new();
        for doc in self.segments.iter().flat_map(|segment| segment.docs.iter()) {
            if default_sidechain.is_some_and(|sidechain| sidechain != doc.is_sidechain) {
                continue;
            }
            let entry = by_session
                .entry((doc.provider, doc.session_id.clone()))
                .or_insert_with(|| RecentAccumulator::new(doc));
            entry.add(doc);
        }
        let mut hits = by_session
            .into_values()
            .map(RecentAccumulator::finish)
            .collect::<Vec<_>>();
        hits.sort_by_key(|hit| Reverse(hit.timestamp));
        hits.truncate(limit);
        hits
    }

    pub fn session_docs(
        &self,
        provider: Provider,
        session_id: &str,
        limit: usize,
    ) -> Vec<MessageDoc> {
        let mut docs = self
            .segments
            .iter()
            .flat_map(|segment| segment.docs.iter())
            .filter(|doc| doc.provider == provider && doc.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        docs.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.source.path.cmp(&b.source.path))
                .then_with(|| a.source.line_number.cmp(&b.source.line_number))
        });
        docs.truncate(limit);
        docs
    }
}

struct RecentAccumulator {
    provider: Provider,
    session_id: String,
    cwd: Option<PathBuf>,
    timestamp: Option<DateTime<Utc>>,
    snippets: Vec<(Option<DateTime<Utc>>, Snippet)>,
}

impl RecentAccumulator {
    fn new(doc: &MessageDoc) -> Self {
        Self {
            provider: doc.provider,
            session_id: doc.session_id.clone(),
            cwd: doc.cwd.clone(),
            timestamp: doc.timestamp,
            snippets: Vec::new(),
        }
    }

    fn add(&mut self, doc: &MessageDoc) {
        if doc.timestamp > self.timestamp {
            self.timestamp = doc.timestamp;
        }
        if self.cwd.is_none() {
            self.cwd = doc.cwd.clone();
        }
        self.snippets.push((
            doc.timestamp,
            Snippet {
                role: doc.role,
                timestamp: doc.timestamp,
                text: snippet_text(&doc.text, ""),
                source: doc.source.clone(),
            },
        ));
    }

    fn finish(mut self) -> SessionHit {
        self.snippets.sort_by_key(|snippet| Reverse(snippet.0));
        SessionHit {
            provider: self.provider,
            session_id: self.session_id,
            cwd: self.cwd,
            timestamp: self.timestamp,
            score: 0.0,
            snippets: self
                .snippets
                .into_iter()
                .take(SNIPPETS_PER_SESSION)
                .map(|(_, snippet)| snippet)
                .collect(),
        }
    }
}

impl PostingCache {
    fn term_postings<'a>(
        &'a mut self,
        segment: &SegmentIndex,
        term: &str,
    ) -> Option<&'a [Posting]> {
        let encoded = segment.term_postings.get(term)?;
        let postings = self
            .term
            .entry(term.to_string())
            .or_insert_with(|| decode_postings(encoded).unwrap_or_default());
        Some(postings.as_slice())
    }

    fn ngram_postings<'a>(
        &'a mut self,
        segment: &SegmentIndex,
        gram: &str,
    ) -> Option<&'a [Posting]> {
        let encoded = segment.ngram_postings.get(gram)?;
        let postings = self
            .ngram
            .entry(gram.to_string())
            .or_insert_with(|| decode_postings(encoded).unwrap_or_default());
        Some(postings.as_slice())
    }
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
                .take(SNIPPETS_PER_SESSION)
                .collect(),
        }
    }
}

fn build_segment(mut docs: Vec<MessageDoc>, max_text_bytes: usize) -> Result<SegmentDisk> {
    for doc in &mut docs {
        doc.text = truncate_utf8(&doc.text, max_text_bytes);
    }
    let mut term_builders: HashMap<String, BTreeMap<u32, PostingBuilder>> = HashMap::new();
    let mut ngram_builders: HashMap<String, BTreeMap<u32, PostingBuilder>> = HashMap::new();

    for (doc_id, doc) in docs.iter().enumerate() {
        let doc_id = doc_id as u32;
        let token_text = index_text(&doc.text, MAX_TOKEN_INDEX_BYTES_PER_DOC);
        for token in tokenizer::tokenize(token_text) {
            term_builders
                .entry(token.text)
                .or_default()
                .entry(doc_id)
                .or_default()
                .positions
                .push(token.position);
        }
        let ngram_text = index_text(&doc.text, MAX_NGRAM_INDEX_BYTES_PER_DOC);
        for (position, gram) in tokenizer::ngrams_for_text(ngram_text)
            .into_iter()
            .enumerate()
        {
            ngram_builders
                .entry(gram)
                .or_default()
                .entry(doc_id)
                .or_default()
                .positions
                .push(position as u32);
        }
    }

    let term_postings = encode_index(term_builders);
    let ngram_postings = encode_index(ngram_builders);
    let fst_terms = build_fst(term_postings.keys())?;
    Ok(SegmentDisk {
        id: new_segment_id(),
        created_at: Utc::now(),
        docs,
        term_postings,
        ngram_postings,
        fst_terms,
    })
}

fn encode_index(
    builders: HashMap<String, BTreeMap<u32, PostingBuilder>>,
) -> BTreeMap<String, Vec<u8>> {
    builders
        .into_iter()
        .map(|(term, docs)| {
            let postings = docs
                .into_iter()
                .map(|(doc_id, builder)| Posting {
                    doc_id,
                    term_freq: builder.positions.len() as u32,
                    positions: builder.positions,
                })
                .collect::<Vec<_>>();
            (term, encode_postings(&postings))
        })
        .collect()
}

fn build_fst<'a>(terms: impl Iterator<Item = &'a String>) -> Result<Vec<u8>> {
    let mut builder = SetBuilder::memory();
    for term in terms {
        builder.insert(term)?;
    }
    Ok(builder.into_inner()?)
}

fn save_segment(paths: &AppPaths, segment: &SegmentDisk) -> Result<SegmentMeta> {
    fs::create_dir_all(&paths.segments_dir)?;
    let file_name = format!("{}.bin", segment.id);
    let path = paths.segments_dir.join(&file_name);
    let bytes = bincode::serialize(segment)?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(SegmentMeta {
        id: segment.id.clone(),
        file_name,
        docs: segment.docs.len(),
        created_at: segment.created_at,
    })
}

fn save_manifest_to(paths: &AppPaths, manifest: &IndexManifest) -> Result<()> {
    fs::create_dir_all(&paths.index_dir)?;
    let raw = serde_json::to_string_pretty(manifest)?;
    fs::write(&paths.manifest_file, raw)
        .with_context(|| format!("failed to write {}", paths.manifest_file.display()))
}

fn paths_for_index_dir(base: &AppPaths, index_dir: PathBuf) -> AppPaths {
    AppPaths {
        config_file: base.config_file.clone(),
        cache_dir: base.cache_dir.clone(),
        segments_dir: index_dir.join("segments"),
        manifest_file: index_dir.join("manifest.json"),
        index_dir,
    }
}

fn replace_index_dir(paths: &AppPaths, temp_paths: &AppPaths) -> Result<()> {
    fs::create_dir_all(&paths.cache_dir)?;
    let old_dir = paths
        .cache_dir
        .join(format!("index-old-{}", new_segment_id()));
    let had_existing = paths.index_dir.exists();
    if had_existing {
        fs::rename(&paths.index_dir, &old_dir).with_context(|| {
            format!(
                "failed to move current index {} to {}",
                paths.index_dir.display(),
                old_dir.display()
            )
        })?;
    }

    let replace_result = fs::rename(&temp_paths.index_dir, &paths.index_dir).with_context(|| {
        format!(
            "failed to install rebuilt index {} to {}",
            temp_paths.index_dir.display(),
            paths.index_dir.display()
        )
    });

    match replace_result {
        Ok(()) => {
            if old_dir.exists() {
                fs::remove_dir_all(&old_dir)
                    .with_context(|| format!("failed to remove old index {}", old_dir.display()))?;
            }
            Ok(())
        }
        Err(error) => {
            if had_existing && old_dir.exists() {
                let _ = fs::rename(&old_dir, &paths.index_dir);
            }
            Err(error)
        }
    }
}

fn load_segment(_meta: SegmentMeta, path: &Path) -> Result<SegmentIndex> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let disk: SegmentDisk = bincode::deserialize(&bytes)
        .with_context(|| format!("failed to decode {}", path.display()))?;
    Ok(SegmentIndex {
        docs: disk.docs,
        term_postings: disk.term_postings,
        ngram_postings: disk.ngram_postings,
    })
}

fn encode_postings(postings: &[Posting]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev_doc = 0u64;
    for posting in postings {
        let doc_id = posting.doc_id as u64;
        varint::encode_u64(doc_id - prev_doc, &mut out);
        prev_doc = doc_id;
        varint::encode_u64(posting.term_freq as u64, &mut out);
        varint::encode_u64(posting.positions.len() as u64, &mut out);
        let mut prev_position = 0u64;
        for position in &posting.positions {
            let position = *position as u64;
            varint::encode_u64(position - prev_position, &mut out);
            prev_position = position;
        }
    }
    out
}

fn decode_postings(bytes: &[u8]) -> Result<Vec<Posting>> {
    let mut postings = Vec::new();
    let mut cursor = 0;
    let mut prev_doc = 0u64;
    while cursor < bytes.len() {
        let doc_delta = varint::decode_u64(bytes, &mut cursor)?;
        let doc_id = prev_doc + doc_delta;
        prev_doc = doc_id;
        let term_freq = varint::decode_u64(bytes, &mut cursor)? as u32;
        let len = varint::decode_u64(bytes, &mut cursor)? as usize;
        let mut positions = Vec::with_capacity(len);
        let mut prev_position = 0u64;
        for _ in 0..len {
            let delta = varint::decode_u64(bytes, &mut cursor)?;
            let position = prev_position + delta;
            prev_position = position;
            positions.push(position as u32);
        }
        postings.push(Posting {
            doc_id: doc_id as u32,
            term_freq,
            positions,
        });
    }
    Ok(postings)
}

fn segment_candidates(
    segment: &SegmentIndex,
    query: &ParsedQuery,
    cache: &mut PostingCache,
) -> BTreeSet<u32> {
    let all_docs = || (0..segment.docs.len() as u32).collect::<BTreeSet<_>>();
    let mut current: Option<BTreeSet<u32>> = None;

    for clause in &query.clauses {
        let clause_docs = clause_candidates(segment, clause, cache);
        current = Some(match current {
            Some(existing) => existing.intersection(&clause_docs).copied().collect(),
            None => clause_docs,
        });
    }

    current.unwrap_or_else(all_docs)
}

fn clause_candidates(
    segment: &SegmentIndex,
    clause: &QueryClause,
    cache: &mut PostingCache,
) -> BTreeSet<u32> {
    match clause {
        QueryClause::Term { raw, tokens } => {
            let mut by_terms: Option<BTreeSet<u32>> = None;
            for token in tokens {
                let docs = postings_doc_set(cache.term_postings(segment, token));
                if docs.is_empty() {
                    by_terms = None;
                    break;
                }
                by_terms = Some(match by_terms {
                    Some(existing) => existing.intersection(&docs).copied().collect(),
                    None => docs,
                });
            }
            by_terms.unwrap_or_else(|| ngram_verified_candidates(segment, cache, raw))
        }
        QueryClause::Prefix { prefix, .. } => {
            let mut docs = BTreeSet::new();
            for term in prefix_terms(&segment.term_postings, prefix) {
                if let Some(postings) = cache.term_postings(segment, &term) {
                    docs.extend(postings.iter().map(|posting| posting.doc_id));
                }
            }
            docs
        }
        QueryClause::Phrase { raw, tokens } => {
            let seed = tokens
                .first()
                .and_then(|token| cache.term_postings(segment, token))
                .map(|postings| postings_doc_set(Some(postings)))
                .unwrap_or_else(|| ngram_verified_candidates(segment, cache, raw));
            seed.into_iter()
                .filter(|doc_id| {
                    segment.docs.get(*doc_id as usize).is_some_and(|doc| {
                        tokenizer::normalize(&doc.text).contains(&tokenizer::normalize(raw))
                    })
                })
                .collect()
        }
    }
}

fn ngram_verified_candidates(
    segment: &SegmentIndex,
    cache: &mut PostingCache,
    raw: &str,
) -> BTreeSet<u32> {
    let grams = tokenizer::ngrams_for_text(raw);
    if grams.is_empty() {
        return BTreeSet::new();
    }
    let mut current: Option<BTreeSet<u32>> = None;
    for gram in grams {
        let docs = postings_doc_set(cache.ngram_postings(segment, &gram));
        if docs.is_empty() {
            return BTreeSet::new();
        }
        current = Some(match current {
            Some(existing) => existing.intersection(&docs).copied().collect(),
            None => docs,
        });
    }
    let needle = tokenizer::normalize(raw);
    current
        .unwrap_or_default()
        .into_iter()
        .filter(|doc_id| {
            segment
                .docs
                .get(*doc_id as usize)
                .is_some_and(|doc| tokenizer::normalize(&doc.text).contains(&needle))
        })
        .collect()
}

fn postings_doc_set(postings: Option<&[Posting]>) -> BTreeSet<u32> {
    postings
        .into_iter()
        .flatten()
        .map(|posting| posting.doc_id)
        .collect()
}

fn prefix_terms(postings: &BTreeMap<String, Vec<u8>>, prefix: &str) -> Vec<String> {
    postings
        .range(prefix.to_string()..)
        .map(|(term, _)| term)
        .take_while(|term| term.starts_with(prefix))
        .cloned()
        .collect()
}

fn matches_filters(doc: &MessageDoc, filters: &QueryFilters) -> bool {
    if filters
        .provider
        .is_some_and(|provider| provider != doc.provider)
    {
        return false;
    }
    if filters.role.is_some_and(|role| role != doc.role) {
        return false;
    }
    if filters
        .sidechain
        .is_some_and(|sidechain| sidechain != doc.is_sidechain)
    {
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

fn score_doc(
    segment: &SegmentIndex,
    cache: &mut PostingCache,
    doc_id: u32,
    doc: &MessageDoc,
    query: &ParsedQuery,
    current_cwd: Option<&PathBuf>,
) -> f32 {
    let mut score = 1.0;
    let total_docs = segment.docs.len().max(1) as f32;
    for clause in &query.clauses {
        match clause {
            QueryClause::Term { raw, tokens } => {
                for token in tokens {
                    if let Some(postings) = cache.term_postings(segment, token)
                        && let Some(posting) = posting_for(Some(postings), doc_id)
                    {
                        let df = postings.len().max(1) as f32;
                        let idf = (1.0 + total_docs / df).ln();
                        score += idf * (1.0 + posting.term_freq as f32).ln();
                    }
                }
                if tokenizer::normalize(&doc.text).contains(&tokenizer::normalize(raw)) {
                    score += 2.0;
                }
            }
            QueryClause::Prefix { prefix, .. } => {
                for term in prefix_terms(&segment.term_postings, prefix) {
                    if posting_for(cache.term_postings(segment, &term), doc_id).is_some() {
                        score += 1.5;
                    }
                }
            }
            QueryClause::Phrase { raw, .. } => {
                if tokenizer::normalize(&doc.text).contains(&tokenizer::normalize(raw)) {
                    score += 5.0;
                }
            }
        }
    }
    if let (Some(current), Some(cwd)) = (current_cwd, doc.cwd.as_ref()) {
        if current == cwd {
            score += 2.0;
        } else if cwd.starts_with(current) || current.starts_with(cwd) {
            score += 1.0;
        }
    }
    if let Some(timestamp) = doc.timestamp {
        let age_days = (Utc::now() - timestamp).num_days().max(0) as f32;
        score += 2.0 / (1.0 + age_days / 30.0);
    }
    score
}

fn posting_for(postings: Option<&[Posting]>, doc_id: u32) -> Option<&Posting> {
    postings?.iter().find(|posting| posting.doc_id == doc_id)
}

fn make_snippet(doc: &MessageDoc, query: &ParsedQuery) -> Snippet {
    let mut needle = None;
    if let Some(clause) = query.clauses.first() {
        match clause {
            QueryClause::Term { raw, .. }
            | QueryClause::Prefix { raw, .. }
            | QueryClause::Phrase { raw, .. } => {
                needle = Some(raw.as_str());
            }
        }
    }
    let text = snippet_text(&doc.text, needle.unwrap_or(""));
    Snippet {
        role: doc.role,
        timestamp: doc.timestamp,
        text,
        source: doc.source.clone(),
    }
}

fn snippet_text(text: &str, needle: &str) -> String {
    let max = 240;
    if text.len() <= max {
        return text.to_string();
    }
    if needle.is_empty() {
        return text.chars().take(max).collect();
    }
    let lower = text.to_lowercase();
    let needle = needle.trim_matches('"').to_lowercase();
    let start = lower
        .find(&needle)
        .map(|idx| idx.saturating_sub(80))
        .unwrap_or(0);
    let start = floor_char_boundary(text, start);
    text[start..].chars().take(max).collect()
}

fn snippet_key(snippet: &Snippet) -> String {
    format!("{}:{}", snippet.role, tokenizer::normalize(&snippet.text))
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = input[..end].to_string();
    text.push_str("\n[truncated]");
    text
}

fn index_text(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn modified_millis(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn new_segment_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("segment-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Role, SourceRef};

    fn doc(session: &str, text: &str, line: u64) -> MessageDoc {
        MessageDoc {
            provider: Provider::Codex,
            session_id: session.to_string(),
            cwd: Some(PathBuf::from("/tmp/project")),
            timestamp: Some(Utc::now()),
            role: Role::User,
            text: text.to_string(),
            source: SourceRef {
                path: PathBuf::from("fixture.jsonl"),
                byte_offset: line * 10,
                line_number: line,
            },
            is_sidechain: false,
        }
    }

    fn search_index(docs: Vec<MessageDoc>) -> SearchIndex {
        let disk = build_segment(docs, 256 * 1024).unwrap();
        SearchIndex {
            manifest: IndexManifest::default(),
            segments: vec![SegmentIndex {
                docs: disk.docs,
                term_postings: disk.term_postings,
                ngram_postings: disk.ngram_postings,
            }],
        }
    }

    #[test]
    fn searches_terms_prefix_phrase_and_cjk() {
        let index = search_index(vec![
            doc("a", "docker compose failure in CamelCaseParser", 1),
            doc("b", "日本語検索のテスト", 2),
        ]);
        assert_eq!(
            index.search("docker", SearchOptions::default())[0].session_id,
            "a"
        );
        assert_eq!(
            index.search("Camel*", SearchOptions::default())[0].session_id,
            "a"
        );
        assert_eq!(
            index.search("日本語", SearchOptions::default())[0].session_id,
            "b"
        );
    }

    #[test]
    fn filters_sidechain_by_default_option() {
        let mut side = doc("side", "hidden text", 1);
        side.is_sidechain = true;
        let index = search_index(vec![side]);
        let options = SearchOptions {
            default_sidechain: Some(false),
            ..SearchOptions::default()
        };
        assert!(index.search("hidden", options).is_empty());
    }

    #[test]
    fn postings_roundtrip() {
        let postings = vec![
            Posting {
                doc_id: 1,
                term_freq: 2,
                positions: vec![1, 4],
            },
            Posting {
                doc_id: 3,
                term_freq: 1,
                positions: vec![2],
            },
        ];
        let encoded = encode_postings(&postings);
        let decoded = decode_postings(&encoded).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[1].doc_id, 3);
    }
}
