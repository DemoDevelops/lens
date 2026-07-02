//! Tantivy full-text backend for the content index.
//!
//! Replaces the FTS5 `chunks` / `chunks_tri` tables. Tantivy builds independent
//! segments across N threads (the one architectural way to use all cores for
//! indexing, since SQLite is single-writer). Two analyzers mirror the two old
//! tables: `en_stem` (porter-equivalent, Tantivy built-in) for the natural-language
//! `symbols` + `content` fields, and a trigram ngram tokenizer for literal-substring
//! / operator queries (mirrors `chunks_tri`).
//!
//! Ranking parity lives in `mod.rs`: the proximity, doc-penalty, and occurrence
//! re-ranks are backend-agnostic Rust over the stored `content`, so only candidate
//! retrieval + the base BM25 score change here. The `symbols` field is weighted
//! [`SYMBOLS_BOOST`]x over `content`, matching the `bm25(chunks, 0,0,5,1)` intent.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{doc, Index as TIndex, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// The `symbols` field is weighted this many times over `content`, matching the
/// FTS5 `bm25(chunks, 0.0, 0.0, 5.0, 1.0)` intent (symbols 5x content) so a query
/// naming a symbol ranks its defining file above files that only mention the term.
const SYMBOLS_BOOST: f32 = 5.0;

/// Trigram width for the substring/operator ngram field (mirrors FTS5 `trigram`).
const NGRAM: usize = 3;

/// Overall writer heap budget per thread. Tantivy's floor is ~15MB/thread; a larger
/// budget means fewer mid-build segment flushes on a bulk index.
const HEAP_PER_THREAD: usize = 64 * 1024 * 1024;

/// Ceiling on writer threads: Tantivy's own `writer()` heuristic caps here, and it
/// bounds the build's peak heap (`threads * HEAP_PER_THREAD`).
const MAX_WRITER_THREADS: usize = 8;

const NGRAM_TOKENIZER: &str = "ngram3";
/// Tantivy built-in: simple tokenizer + lowercase + English (porter) stemmer.
const STEM_TOKENIZER: &str = "en_stem";

/// How long to retry acquiring the single Tantivy writer lock before giving up
/// (another process — `lens warmup`/`watch` while the server runs — may hold it).
const WRITER_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Schema field handles. `Field` is `Copy`, so this is cheap to clone.
#[derive(Clone)]
pub(crate) struct Fields {
    pub path: Field,
    pub chunk_id: Field,
    pub symbols: Field,
    pub content: Field,
    pub content_ng: Field,
}

/// A Tantivy-backed FTS store: one index directory, one reusable reader, the schema
/// field handles. Wrapped in an `Arc` by [`crate::index::Index`] so the handle is
/// cheap to share across async tasks.
pub(crate) struct TantivyStore {
    index: TIndex,
    reader: IndexReader,
    pub fields: Fields,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    // path + chunk_id: raw, untokenized (STRING = one token = the whole value) and
    // stored. STRING lets delete-by-term (incremental re-index, session records) and
    // distinct-path enumeration (prune) work on the exact key.
    let path = b.add_text_field("path", STRING | STORED);
    let chunk_id = b.add_text_field("chunk_id", STRING | STORED);
    // symbols: stemmed/lowercased, index-only (never read back), freqs for BM25.
    let symbols = b.add_text_field(
        "symbols",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(STEM_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqs),
        ),
    );
    // content: stemmed/lowercased AND stored (the Rust re-rank + snippet read it).
    let content = b.add_text_field(
        "content",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(STEM_TOKENIZER)
                    .set_index_option(IndexRecordOption::WithFreqs),
            )
            .set_stored(),
    );
    // content_ng: the same content under a trigram tokenizer, for literal substring /
    // operator queries the stemmed path strips (`std::fs`, `&str`).
    let content_ng = b.add_text_field(
        "content_ng",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(NGRAM_TOKENIZER)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    let schema = b.build();
    (
        schema,
        Fields {
            path,
            chunk_id,
            symbols,
            content,
            content_ng,
        },
    )
}

/// Register the trigram tokenizer (`en_stem` ships registered by default).
fn register_tokenizers(index: &TIndex) {
    let ngram = TextAnalyzer::builder(
        NgramTokenizer::new(NGRAM, NGRAM, false).expect("valid ngram bounds"),
    )
    .filter(LowerCaser)
    .build();
    index.tokenizers().register(NGRAM_TOKENIZER, ngram);
}

impl TantivyStore {
    /// Open the index under `fts_dir`, creating it if absent. A schema mismatch
    /// (format drift) wipes and recreates the directory so a stale index is rebuilt.
    pub fn open(fts_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(fts_dir)
            .with_context(|| format!("creating fts dir {}", fts_dir.display()))?;
        let (schema, fields) = build_schema();
        let index = match TIndex::open_in_dir(fts_dir) {
            Ok(idx) if idx.schema() == schema => idx,
            _ => {
                clear_dir(fts_dir)?;
                TIndex::create_in_dir(fts_dir, schema.clone())
                    .with_context(|| format!("creating tantivy index in {}", fts_dir.display()))?
            }
        };
        register_tokenizers(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("building tantivy reader")?;
        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    /// Acquire the single exclusive writer, `num_threads`-wide, retrying briefly on a
    /// lock held by another process before surfacing the error to a best-effort caller.
    pub fn writer(&self, num_threads: usize) -> Result<IndexWriter> {
        let threads = num_threads.clamp(1, MAX_WRITER_THREADS);
        let heap = threads * HEAP_PER_THREAD;
        let deadline = Instant::now() + WRITER_LOCK_TIMEOUT;
        loop {
            match self.index.writer_with_num_threads(threads, heap) {
                Ok(w) => return Ok(w),
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(anyhow::Error::new(e).context("acquiring tantivy writer"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// Add one chunk. `content` is indexed under both the stemmed and trigram fields
    /// and stored once.
    pub fn add_chunk(
        &self,
        writer: &IndexWriter,
        path: &str,
        chunk_id: &str,
        symbols: &str,
        content: &str,
    ) -> Result<()> {
        writer
            .add_document(doc!(
                self.fields.path => path,
                self.fields.chunk_id => chunk_id,
                self.fields.symbols => symbols,
                self.fields.content => content,
                self.fields.content_ng => content,
            ))
            .context("adding tantivy document")?;
        Ok(())
    }

    pub fn delete_path(&self, writer: &IndexWriter, path: &str) {
        writer.delete_term(Term::from_field_text(self.fields.path, path));
    }

    pub fn delete_chunk_id(&self, writer: &IndexWriter, chunk_id: &str) {
        writer.delete_term(Term::from_field_text(self.fields.chunk_id, chunk_id));
    }

    /// Reload the reader so committed writes are visible to later searches.
    pub fn reload(&self) -> Result<()> {
        self.reader.reload().context("reloading tantivy reader")
    }

    /// Live (non-deleted) document count, for `lens_stats` / readiness.
    pub fn num_docs(&self) -> Result<u64> {
        self.reader.reload().context("reloading tantivy reader")?;
        Ok(self.reader.searcher().num_docs())
    }

    /// Distinct `path` values currently in the index (via the path field's term
    /// dictionary). May include a path whose docs are all delete-marked but not yet
    /// merged away; a redundant delete of such a path is a harmless no-op.
    pub fn distinct_paths(&self) -> Result<Vec<String>> {
        self.reader.reload().context("reloading tantivy reader")?;
        let searcher = self.reader.searcher();
        let mut out = std::collections::HashSet::new();
        for seg in searcher.segment_readers() {
            let inv = seg.inverted_index(self.fields.path)?;
            let mut stream = inv.terms().stream()?;
            while stream.advance() {
                if let Ok(s) = std::str::from_utf8(stream.key()) {
                    out.insert(s.to_string());
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Ranked candidate pool: OR-join of the query's stemmed tokens across `symbols`
    /// (boosted [`SYMBOLS_BOOST`]x) and `content`, top `fetch` by Tantivy BM25.
    /// Returns `(path, chunk_id, content, base_score)`; `mod.rs` applies the doc
    /// penalty + proximity re-rank on top.
    pub fn ranked_candidates(
        &self,
        query: &str,
        fetch: usize,
    ) -> Result<Vec<(String, String, String, f64)>> {
        self.reader.reload().context("reloading tantivy reader")?;
        let searcher = self.reader.searcher();
        let terms = self.analyze(STEM_TOKENIZER, query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(terms.len() * 2);
        for t in &terms {
            let symq = BoostQuery::new(
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.symbols, t),
                    IndexRecordOption::WithFreqs,
                )),
                SYMBOLS_BOOST,
            );
            let conq = TermQuery::new(
                Term::from_field_text(self.fields.content, t),
                IndexRecordOption::WithFreqs,
            );
            clauses.push((Occur::Should, Box::new(symq)));
            clauses.push((Occur::Should, Box::new(conq)));
        }
        let bq = BooleanQuery::new(clauses);
        let top = searcher.search(&bq, &TopDocs::with_limit(fetch.max(1)).order_by_score())?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            out.push((
                self.stored(&d, self.fields.path),
                self.stored(&d, self.fields.chunk_id),
                self.stored(&d, self.fields.content),
                score as f64,
            ));
        }
        Ok(out)
    }

    /// Structural candidate pool for a literal-substring / operator query. `>= NGRAM`
    /// chars use the trigram field (an AND of the query's trigrams — a substring
    /// superset `mod.rs` filters exactly); shorter operators scan all docs (mirrors
    /// the FTS5 `LIKE` fallback the trigram index cannot serve). Returns
    /// `(path, chunk_id, content)`.
    pub fn structural_candidates(
        &self,
        query: &str,
        fetch: usize,
    ) -> Result<Vec<(String, String, String)>> {
        self.reader.reload().context("reloading tantivy reader")?;
        let searcher = self.reader.searcher();
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        if q.chars().count() >= NGRAM {
            let grams = self.analyze(NGRAM_TOKENIZER, q);
            if grams.is_empty() {
                return Ok(Vec::new());
            }
            let clauses: Vec<(Occur, Box<dyn Query>)> = grams
                .into_iter()
                .map(|g| {
                    let tq = TermQuery::new(
                        Term::from_field_text(self.fields.content_ng, &g),
                        IndexRecordOption::Basic,
                    );
                    (Occur::Must, Box::new(tq) as Box<dyn Query>)
                })
                .collect();
            let bq = BooleanQuery::new(clauses);
            let top =
                searcher.search(&bq, &TopDocs::with_limit(fetch.max(NGRAM * 16)).order_by_score())?;
            let mut out = Vec::with_capacity(top.len());
            for (_score, addr) in top {
                let d: TantivyDocument = searcher.doc(addr)?;
                out.push((
                    self.stored(&d, self.fields.path),
                    self.stored(&d, self.fields.chunk_id),
                    self.stored(&d, self.fields.content),
                ));
            }
            Ok(out)
        } else {
            // No trigram for < NGRAM chars: scan every doc and keep the literal
            // substring matches (case-insensitive), same shape as `content LIKE`.
            let needle = q.to_lowercase();
            let all = searcher.search(&AllQuery, &DocSetCollector)?;
            let mut out = Vec::new();
            for addr in all {
                let d: TantivyDocument = searcher.doc(addr)?;
                let content = self.stored(&d, self.fields.content);
                if content.to_lowercase().contains(&needle) {
                    out.push((
                        self.stored(&d, self.fields.path),
                        self.stored(&d, self.fields.chunk_id),
                        content,
                    ));
                }
            }
            Ok(out)
        }
    }

    /// Run `text` through a registered tokenizer, collecting its distinct output
    /// tokens in order (query-side stemming / gramming that matches the index).
    fn analyze(&self, tokenizer: &str, text: &str) -> Vec<String> {
        let mut tk = match self.index.tokenizers().get(tokenizer) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut stream = tk.token_stream(text);
        let mut out: Vec<String> = Vec::new();
        while stream.advance() {
            let t = &stream.token().text;
            if !out.iter().any(|e| e == t) {
                out.push(t.clone());
            }
        }
        out
    }

    fn stored(&self, d: &TantivyDocument, f: Field) -> String {
        d.get_first(f)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

/// Remove every entry under `dir` (used to wipe a schema-drifted index before
/// recreating it). The directory itself is kept.
fn clear_dir(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            std::fs::remove_dir_all(&p).ok();
        } else {
            std::fs::remove_file(&p).ok();
        }
    }
    Ok(())
}
