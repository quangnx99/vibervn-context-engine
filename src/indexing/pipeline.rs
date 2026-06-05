use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use surrealdb::Surreal;
use surrealdb::engine::local::Db;
use tracing::{debug, info, warn};

use crate::embedding::InputType;
use crate::embedding::voyage::{MAX_BATCH_SIZE, VoyageClient};
use crate::indexing::ProgressHandle;
use crate::indexing::tracker::{ChangeKind, FileChange, stat_file};
use crate::indexing::walker::walk_repo;
use crate::parsing::parse_file;
use crate::parsing::relations::{EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};
use crate::store::ops::{FileMeta, delete_all_data, delete_file_data, get_all_file_meta, upsert_file_meta};
use crate::vector::{ChunkId, VectorIndex};

/// Batch size for DB writes — keeps per-query payload small and avoids the
/// gigabyte-sized transaction that caused 3 GB RAM spikes on large repos.
const WRITE_BATCH_SIZE: usize = 512;

/// A chunk row ready for bulk INSERT via native SurrealDB parameter binding.
/// Using `Vec<f32>` (not a text-formatted string) means the driver serialises
/// the embedding as a CBOR array — no float-token parsing by the query engine.
#[derive(Serialize)]
struct ChunkRecord {
    file: String,
    line_start: i64,
    line_end: i64,
    content: String,
    embedding: Vec<f32>,
    symbol_ref: Option<String>,
}

pub struct IndexPipelineStats {
    pub indexed_files: u64,
    pub total_files: u64,
}

/// Runs the parse → embed → store pipeline for one repo.
pub struct IndexPipeline {
    repo: String,
    voyage: Option<VoyageClient>,
}

impl IndexPipeline {
    pub fn new(repo: String, voyage: Option<VoyageClient>) -> Self {
        Self { repo, voyage }
    }

    /// Run the pipeline against the shared `db` handle.
    /// - `changes = None` → incremental scan (detect changes from mtime).
    /// - `changes = Some(list)` → process only the given file changes.
    /// - `force_rebuild = true` → clear and re-embed everything, ignoring staleness.
    /// - `progress` → optional handle for reporting live progress to the status map.
    pub async fn run(
        &self,
        db: &Surreal<Db>,
        changes: Option<Vec<FileChange>>,
        force_rebuild: bool,
        vector_index: Option<&tokio::sync::RwLock<VectorIndex>>,
        progress: Option<ProgressHandle>,
    ) -> Result<IndexPipelineStats> {
        // Check if first run (no file_meta at all).
        let stored_meta = get_all_file_meta(db, &self.repo).await?;
        let is_first_run = stored_meta.is_empty();

        let total_files = walk_repo(&self.repo).len() as u64;

        if is_first_run || force_rebuild {
            if force_rebuild && !is_first_run {
                info!(repo = %self.repo, "forced full rebuild");
            } else {
                info!(repo = %self.repo, "first run — full rebuild");
            }
            let new_vectors = self.full_rebuild(db, progress.as_ref()).await?;
            if let Some(vi) = vector_index {
                let mut guard = vi.write().await;
                guard.remove_repo(&self.repo);
                guard.insert(&new_vectors);
            }
            let indexed = get_all_file_meta(db, &self.repo).await?.len() as u64;
            return Ok(IndexPipelineStats { indexed_files: indexed, total_files });
        }

        // Incremental run.
        let file_changes = match changes {
            Some(explicit) => explicit,
            None => {
                // Detect via mtime comparison.
                let all_files = walk_repo(&self.repo);
                let meta_map: HashMap<String, (i64, i64)> = stored_meta
                    .iter()
                    .map(|m| (m.path.clone(), (m.mtime, m.size)))
                    .collect();
                crate::indexing::tracker::detect_changes(&all_files, &meta_map)
            }
        };

        if file_changes.is_empty() {
            debug!(repo = %self.repo, "no changes detected");
            let indexed = stored_meta.len() as u64;
            return Ok(IndexPipelineStats { indexed_files: indexed, total_files });
        }

        info!(repo = %self.repo, changes = file_changes.len(), "incremental index");
        let (removed_files, new_vectors) = self.incremental_run(db, file_changes, progress.as_ref()).await?;

        if let Some(vi) = vector_index {
            let mut guard = vi.write().await;
            for file in &removed_files {
                guard.remove_file(file);
            }
            guard.insert(&new_vectors);
        }

        let indexed = get_all_file_meta(db, &self.repo).await?.len() as u64;
        Ok(IndexPipelineStats { indexed_files: indexed, total_files })
    }

    // ─── Full rebuild ─────────────────────────────────────────────────────

    /// Returns (chunk_id, embedding) pairs for VectorIndex insertion.
    async fn full_rebuild(&self, db: &Surreal<Db>, progress: Option<&ProgressHandle>) -> Result<Vec<(ChunkId, Vec<f32>)>> {
        // 1. Walk all files.
        let all_files = walk_repo(&self.repo);
        info!(repo = %self.repo, file_count = all_files.len(), "walking repo for full rebuild");

        // 2. Parse all files.
        let parse_results = parse_all_files_parallel(&all_files);

        // 3. Collect symbols, chunks, edges.
        let mut all_symbols: Vec<Symbol> = Vec::new();
        let mut all_chunks_by_file: Vec<(String, Vec<crate::parsing::chunker::Chunk>)> = Vec::new();
        let mut all_edges: Vec<RawEdge> = Vec::new();

        for (file, pr) in &parse_results {
            all_symbols.extend(pr.symbols.iter().cloned());
            all_chunks_by_file.push((file.clone(), pr.chunks.clone()));
            all_edges.extend(pr.edges.iter().cloned());
        }

        // 4. Build symbol index for cross-file resolution.
        let symbol_index = build_symbol_index(&all_symbols);

        // 5. Embed all chunks (outside transaction — network I/O).
        let embeddings = self.embed_all_chunks(&all_chunks_by_file, progress).await?;

        // 6. Resolve edges.
        let resolved_edges = resolve_edges(&all_edges, &symbol_index);

        // 7. Compute file stats before the transaction.
        let file_stats: Vec<(String, i64, i64)> = all_files
            .iter()
            .filter_map(|f| stat_file(f).map(|s| (f.clone(), s.mtime, s.size)))
            .collect();

        // 8. Write in independent batches (no outer transaction).
        //
        // Order is crash-safe: delete → symbols → edges → chunks → file_meta.
        // If the process dies before file_meta is written the next run detects
        // missing/stale meta and rebuilds — the index is never left half-done.

        // Delete everything first.
        delete_all_data(db).await.context("full_rebuild: delete_all_data")?;

        // Symbols — flush every WRITE_BATCH_SIZE statements.
        flush_symbol_batches(db, &all_symbols).await.context("full_rebuild: symbols")?;

        // Edges — flush every WRITE_BATCH_SIZE statements.
        flush_edge_batches(db, &resolved_edges).await.context("full_rebuild: edges")?;

        // Chunks — native Vec<f32> binding, RETURN NONE, batched.
        let mut emb_iter = embeddings.iter();
        let mut chunk_vectors: Vec<(ChunkId, Vec<f32>)> = Vec::new();
        let mut chunk_batch: Vec<ChunkRecord> = Vec::with_capacity(WRITE_BATCH_SIZE);

        for (_file, chunks) in &all_chunks_by_file {
            for chunk in chunks {
                let emb = emb_iter.next().cloned().unwrap_or_default();
                chunk_vectors.push((
                    ChunkId {
                        file: chunk.file.clone(),
                        line_start: chunk.line_start,
                        line_end: chunk.line_end,
                    },
                    emb.clone(),
                ));
                chunk_batch.push(ChunkRecord {
                    file: chunk.file.clone(),
                    line_start: chunk.line_start as i64,
                    line_end: chunk.line_end as i64,
                    content: chunk.content.clone(),
                    embedding: emb,
                    symbol_ref: chunk.symbol_ref.as_ref().map(|fqn| format!("symbol:⟨{fqn}⟩")),
                });

                if chunk_batch.len() >= WRITE_BATCH_SIZE {
                    flush_chunk_batch(db, std::mem::take(&mut chunk_batch)).await.context("full_rebuild: chunk batch")?;
                }
            }
        }
        if !chunk_batch.is_empty() {
            flush_chunk_batch(db, chunk_batch).await.context("full_rebuild: chunk batch (tail)")?;
        }
        for (path, mtime, size) in &file_stats {
            upsert_file_meta(db, &FileMeta {
                path: path.clone(),
                mtime: *mtime,
                size: *size,
                repo: self.repo.clone(),
            }).await.context("full_rebuild: upsert_file_meta")?;
        }

        Ok(chunk_vectors)
    }

    // ─── Incremental run ──────────────────────────────────────────────────

    /// Returns (files_removed, new_chunk_vectors) for VectorIndex update.
    async fn incremental_run(
        &self,
        db: &Surreal<Db>,
        changes: Vec<FileChange>,
        progress: Option<&ProgressHandle>,
    ) -> Result<(Vec<String>, Vec<(ChunkId, Vec<f32>)>)> {
        // Separate added/modified from deleted.
        let to_process: Vec<String> = changes
            .iter()
            .filter(|c| c.kind != ChangeKind::Deleted)
            .map(|c| c.path.clone())
            .collect();
        let to_delete: Vec<String> = changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Deleted)
            .map(|c| c.path.clone())
            .collect();

        // All files whose old data must be purged.
        let all_affected: Vec<String> = to_delete
            .iter()
            .chain(to_process.iter())
            .cloned()
            .collect();

        // Parse changed files.
        let parse_results = parse_all_files_parallel(&to_process);

        let mut all_symbols: Vec<Symbol> = Vec::new();
        let mut all_chunks_by_file: Vec<(String, Vec<crate::parsing::chunker::Chunk>)> = Vec::new();
        let mut all_edges: Vec<RawEdge> = Vec::new();

        for (file, pr) in &parse_results {
            all_symbols.extend(pr.symbols.iter().cloned());
            all_chunks_by_file.push((file.clone(), pr.chunks.clone()));
            all_edges.extend(pr.edges.iter().cloned());
        }

        // Build symbol index from new symbols + existing DB symbols.
        let mut symbol_index = build_symbol_index(&all_symbols);
        let db_symbols = query_all_symbols_from_db(db).await?;
        for sym in db_symbols {
            symbol_index.entry(sym.name.clone()).or_default().push(sym);
        }

        // Embed chunks outside transaction.
        let embeddings = self.embed_all_chunks(&all_chunks_by_file, progress).await?;

        // Resolve edges.
        let resolved_edges = resolve_edges(&all_edges, &symbol_index);

        // Compute file stats before the transaction.
        let file_stats: Vec<(String, i64, i64)> = to_process
            .iter()
            .filter_map(|f| stat_file(f).map(|s| (f.clone(), s.mtime, s.size)))
            .collect();

        // Write in independent batches — crash-safe order:
        // delete → symbols → edges → chunks → file_meta.

        // Delete old data for all affected files.
        for file in &all_affected {
            delete_file_data(db, file).await.context("incremental_run: delete_file_data")?;
        }

        // Insert new symbols.
        flush_symbol_batches(db, &all_symbols).await.context("incremental_run: symbols")?;

        // Insert edges.
        flush_edge_batches(db, &resolved_edges).await.context("incremental_run: edges")?;

        // Insert chunks — native Vec<f32>, RETURN NONE, batched.
        let mut emb_iter = embeddings.iter();
        let mut chunk_vectors: Vec<(ChunkId, Vec<f32>)> = Vec::new();
        let mut chunk_batch: Vec<ChunkRecord> = Vec::with_capacity(WRITE_BATCH_SIZE);

        for (_file, chunks) in &all_chunks_by_file {
            for chunk in chunks {
                let emb = emb_iter.next().cloned().unwrap_or_default();
                chunk_vectors.push((
                    ChunkId {
                        file: chunk.file.clone(),
                        line_start: chunk.line_start,
                        line_end: chunk.line_end,
                    },
                    emb.clone(),
                ));
                chunk_batch.push(ChunkRecord {
                    file: chunk.file.clone(),
                    line_start: chunk.line_start as i64,
                    line_end: chunk.line_end as i64,
                    content: chunk.content.clone(),
                    embedding: emb,
                    symbol_ref: chunk.symbol_ref.as_ref().map(|fqn| format!("symbol:⟨{fqn}⟩")),
                });

                if chunk_batch.len() >= WRITE_BATCH_SIZE {
                    flush_chunk_batch(db, std::mem::take(&mut chunk_batch)).await.context("incremental_run: chunk batch")?;
                }
            }
        }
        if !chunk_batch.is_empty() {
            flush_chunk_batch(db, chunk_batch).await.context("incremental_run: chunk batch (tail)")?;
        }

        // file_meta for added/modified files — written LAST (crash-safety anchor).
        for (path, mtime, size) in &file_stats {
            upsert_file_meta(db, &FileMeta {
                path: path.clone(),
                mtime: *mtime,
                size: *size,
                repo: self.repo.clone(),
            }).await.context("incremental_run: upsert_file_meta")?;
        }

        // Delete file_meta for deleted files.
        for file in &to_delete {
            let escaped = escape_surreal(file);
            db.query(format!(
                "DELETE FROM file_meta WHERE path = '{escaped}'"
            ))
            .await
            .context("incremental_run: delete file_meta for deleted file")?;
        }

        Ok((all_affected, chunk_vectors))
    }

    // ─── Embedding helper ─────────────────────────────────────────────────

    /// Embed all chunks, reporting per-batch progress via `progress`.
    ///
    /// Progress advances at embedding batch boundaries (every `MAX_BATCH_SIZE`
    /// chunks). The numerator counts files whose last chunk has been embedded,
    /// using a per-file cumulative prefix over the flattened chunk list so the
    /// denominator and numerator always use the same file set and the bar
    /// reaches exactly 100%.
    async fn embed_all_chunks(
        &self,
        chunks_by_file: &[(String, Vec<crate::parsing::chunker::Chunk>)],
        progress: Option<&ProgressHandle>,
    ) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<String> = chunks_by_file
            .iter()
            .flat_map(|(_, chunks)| chunks.iter().map(|c| c.content.clone()))
            .collect();

        if texts.is_empty() {
            // Nothing to embed — report total immediately so the bar completes.
            if let Some(ph) = progress {
                let total = chunks_by_file.len() as u64;
                ph.set_run_total(total).await;
                ph.set_processed(total).await;
            }
            return Ok(vec![]);
        }

        // Precompute cumulative chunk-end index for each file so we can map
        // "chunks done so far" → "files fully embedded".
        // cumulative[i] = index of the last chunk of file i in the flat list (exclusive end).
        let mut cumulative: Vec<usize> = Vec::with_capacity(chunks_by_file.len());
        let mut running = 0usize;
        for (_, chunks) in chunks_by_file {
            running += chunks.len();
            cumulative.push(running);
        }
        let total_files = chunks_by_file.len() as u64;

        // Report the denominator once the file set is known, before any I/O.
        if let Some(ph) = progress {
            ph.set_run_total(total_files).await;
        }

        match &self.voyage {
            Some(client) => {
                info!(count = texts.len(), "embedding chunks");
                let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
                let mut done: usize = 0;

                for batch in texts.chunks(MAX_BATCH_SIZE) {
                    let batch_vec: Vec<String> = batch.to_vec();
                    let embeddings = client.embed_batch(&batch_vec, InputType::Document).await?;
                    done += embeddings.len();
                    all_embeddings.extend(embeddings);

                    // Count how many files are fully embedded (all their chunks done).
                    if let Some(ph) = progress {
                        // Binary-search for the rightmost file whose cumulative end <= done.
                        let completed_files = cumulative.partition_point(|&end| end <= done) as u64;
                        ph.set_processed(completed_files).await;
                    }
                }

                Ok(all_embeddings)
            }
            None => {
                warn!("no embedding client configured; storing empty embeddings");
                // No network I/O — mark everything complete immediately.
                if let Some(ph) = progress {
                    ph.set_processed(total_files).await;
                }
                Ok(vec![vec![]; texts.len()])
            }
        }
    }
}

// ─── Parallel parsing ─────────────────────────────────────────────────────

fn parse_all_files_parallel(
    files: &[String],
) -> Vec<(String, crate::parsing::ParseResult)> {
    use rayon::prelude::*;

    files
        .par_iter()
        .filter_map(|file| {
            let source = match std::fs::read_to_string(file) {
                Ok(s) => s,
                Err(e) => {
                    warn!(file = %file, error = %e, "failed to read file");
                    return None;
                }
            };
            let result = parse_file(file, &source);
            Some((file.clone(), result))
        })
        .collect()
}

// ─── Symbol index helpers ─────────────────────────────────────────────────

fn build_symbol_index(symbols: &[Symbol]) -> HashMap<String, Vec<QualifiedSymbol>> {
    let mut index: HashMap<String, Vec<QualifiedSymbol>> = HashMap::new();
    for sym in symbols {
        index
            .entry(sym.qualified.name.clone())
            .or_default()
            .push(sym.qualified.clone());
    }
    index
}

async fn query_all_symbols_from_db(
    db: &Surreal<Db>,
) -> Result<Vec<QualifiedSymbol>> {
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Row {
        file: String,
        name: String,
    }

    let rows: Vec<Row> = db
        .query("SELECT file, name FROM symbol")
        .await
        .context("query all symbols")?
        .take(0)?;

    Ok(rows
        .into_iter()
        .map(|r| QualifiedSymbol {
            file: r.file,
            scope_path: vec![],
            name: r.name,
        })
        .collect())
}

// ─── Edge resolution ──────────────────────────────────────────────────────

fn resolve_edges(
    edges: &[RawEdge],
    symbol_index: &HashMap<String, Vec<QualifiedSymbol>>,
) -> Vec<(QualifiedSymbol, QualifiedSymbol, EdgeKind, u32)> {
    let mut resolved = Vec::new();

    for edge in edges {
        let to = match &edge.to {
            EdgeTarget::Resolved(qs) => qs.clone(),
            EdgeTarget::Unresolved { name, .. } => {
                match symbol_index.get(name) {
                    Some(candidates) if !candidates.is_empty() => {
                        let same_file = candidates
                            .iter()
                            .find(|c| c.file == edge.from.file);
                        same_file
                            .or_else(|| candidates.first())
                            .cloned()
                            .unwrap()
                    }
                    _ => {
                        debug!(name = %name, "dropping unresolved edge");
                        continue;
                    }
                }
            }
        };

        resolved.push((edge.from.clone(), to, edge.kind.clone(), edge.line));
    }

    resolved
}

// ─── SurrealQL escaping ───────────────────────────────────────────────────

/// Escape a string for safe embedding in a SurrealQL single-quoted literal.
/// Handles backslashes (must be first) and single quotes.
fn escape_surreal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

// ─── Batch write helpers ──────────────────────────────────────────────────

/// Flush a batch of chunk records via a native-bind INSERT (no text float
/// serialisation). Using `RETURN NONE` prevents SurrealDB from echoing the
/// full embedding vectors back, keeping response allocation small.
async fn flush_chunk_batch(db: &Surreal<Db>, batch: Vec<ChunkRecord>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    db.query("INSERT INTO chunk $data RETURN NONE")
        .bind(("data", batch))
        .await
        .context("flush_chunk_batch")?;
    Ok(())
}

/// Flush symbols in batches of WRITE_BATCH_SIZE using text-query statements.
/// Symbols carry no large float payload so text is fine; batching prevents
/// a single multi-thousand-statement query.
async fn flush_symbol_batches(
    db: &Surreal<Db>,
    symbols: &[Symbol],
) -> Result<()> {
    for chunk in symbols.chunks(WRITE_BATCH_SIZE) {
        let mut batch = String::new();
        for sym in chunk {
            append_upsert_symbol(&mut batch, sym);
        }
        if !batch.is_empty() {
            db.query(&batch).await.context("flush_symbol_batches")?;
        }
    }
    Ok(())
}

/// Flush edges in batches of WRITE_BATCH_SIZE using text-query statements.
async fn flush_edge_batches(
    db: &Surreal<Db>,
    edges: &[(QualifiedSymbol, QualifiedSymbol, EdgeKind, u32)],
) -> Result<()> {
    for chunk in edges.chunks(WRITE_BATCH_SIZE) {
        let mut batch = String::new();
        for (from, to, kind, line) in chunk {
            append_insert_edge(&mut batch, from, to, kind, *line);
        }
        if !batch.is_empty() {
            db.query(&batch).await.context("flush_edge_batches")?;
        }
    }
    Ok(())
}

// ─── Transaction query builders ───────────────────────────────────────────

/// Append an UPSERT statement for `sym`.
fn append_upsert_symbol(txn: &mut String, sym: &Symbol) {
    use crate::store::ops::kind_to_str;

    let fqn = escape_surreal(&sym.qualified.fqn());
    let name = escape_surreal(&sym.qualified.name);
    let kind = kind_to_str(&sym.kind);
    let file = escape_surreal(&sym.qualified.file);
    let ls = sym.line_start as i64;
    let le = sym.line_end as i64;
    let sig = sym
        .signature
        .as_deref()
        .map(|s| format!("'{}'", escape_surreal(s)))
        .unwrap_or_else(|| "NONE".to_string());
    let parent = sym
        .parent_fqn
        .as_deref()
        .map(|p| format!("'symbol:⟨{}⟩'", escape_surreal(p)))
        .unwrap_or_else(|| "NONE".to_string());

    txn.push_str(&format!(
        "UPSERT symbol:`⟨{fqn}⟩` SET \
         name = '{name}', kind = '{kind}', file = '{file}', \
         line_start = {ls}, line_end = {le}, \
         signature = {sig}, parent = {parent};\n"
    ));
}

/// Append a RELATE statement for an edge.
fn append_insert_edge(
    txn: &mut String,
    from: &QualifiedSymbol,
    to: &QualifiedSymbol,
    kind: &EdgeKind,
    line: u32,
) {
    let from_fqn = escape_surreal(&from.fqn());
    let to_fqn = escape_surreal(&to.fqn());
    let in_file = escape_surreal(&from.file);
    let out_file = escape_surreal(&to.file);
    let table = match kind {
        EdgeKind::Calls => "calls",
        EdgeKind::Uses => "uses",
        EdgeKind::Imports => "imports",
        EdgeKind::Contains => "contains",
        EdgeKind::Implements => "implements",
    };

    if matches!(kind, EdgeKind::Calls) {
        txn.push_str(&format!(
            "RELATE symbol:`⟨{from_fqn}⟩`->{table}->symbol:`⟨{to_fqn}⟩` \
             SET line = {line}, in_file = '{in_file}', out_file = '{out_file}';\n"
        ));
    } else {
        txn.push_str(&format!(
            "RELATE symbol:`⟨{from_fqn}⟩`->{table}->symbol:`⟨{to_fqn}⟩` \
             SET in_file = '{in_file}', out_file = '{out_file}';\n"
        ));
    }
}

// ─── End-to-end pipeline regression tests ────────────────────────────────
//
// Drives the real full_rebuild write path end-to-end (voyage = None so no
// network) and asserts that chunks, files, and symbols all persist.
#[cfg(test)]
mod end_to_end_persist {
    use super::*;
    use crate::store::open_db;
    use crate::store::ops::{count_chunks, count_indexed_files, count_symbols};
    use tempfile::TempDir;

    /// Write a tiny Rust source file into `dir` and return its absolute path.
    fn write_test_file(dir: &std::path::Path) -> String {
        let path = dir.join("sample.rs");
        std::fs::write(
            &path,
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\nfn subtract(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
        )
        .expect("write test file");
        path.to_str().unwrap().replace('\\', "/")
    }

    /// Full-rebuild of the real context-engine-rs source tree (voyage=None).
    /// This exercises the SAME code path and file set as the live failing run.
    #[tokio::test]
    async fn full_rebuild_real_source_tree_voyage_none() {
        let home = TempDir::new().unwrap();
        let repo = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
        println!("REAL-TREE PROBE: repo = {repo}");

        let db = open_db(home.path(), &repo).await.expect("open db");
        let pipeline = IndexPipeline::new(repo.clone(), None);

        let result = pipeline.run(&db, None, true, None, None).await;
        println!("REAL-TREE PROBE: result = {:?}", result.as_ref().map(|s| (s.indexed_files, s.total_files)));

        let chunks = count_chunks(&db).await.unwrap();
        let symbols = count_symbols(&db).await.unwrap();
        let files = count_indexed_files(&db, &repo).await.unwrap();
        println!("REAL-TREE PROBE: chunks={chunks}, symbols={symbols}, files={files}");

        assert!(result.is_ok(), "full_rebuild of real source tree must succeed (got: {:?})", result.err());
        assert!(chunks > 0, "must have chunks after full_rebuild of real source tree");
        assert!(files > 0, "must have indexed files");
    }

    /// Full-rebuild through the real IndexPipeline (voyage=None) must persist
    /// chunks, indexed files, and symbols.
    #[tokio::test]
    async fn full_rebuild_persists_chunks_files_symbols() {
        let home = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();

        let _file_path = write_test_file(repo_dir.path());
        let repo = repo_dir.path().to_str().unwrap().replace('\\', "/");

        let db = open_db(home.path(), &repo).await.expect("open db");
        let pipeline = IndexPipeline::new(repo.clone(), None);

        let stats = pipeline
            .run(&db, None, true, None, None)
            .await
            .expect("full_rebuild must succeed");

        let chunks = count_chunks(&db).await.unwrap();
        let files = count_indexed_files(&db, &repo).await.unwrap();
        let symbols = count_symbols(&db).await.unwrap();

        println!("STEP3 — indexed_files={}, total_files={}", stats.indexed_files, stats.total_files);
        println!("STEP3 — chunks={chunks}, files={files}, symbols={symbols}");

        assert!(chunks > 0,
            "chunks must be > 0 after full_rebuild (got {chunks}); batched write path failed");
        assert!(files > 0,
            "indexed files must be > 0 after full_rebuild (got {files})");
        assert!(symbols > 0,
            "symbols must be > 0 after full_rebuild (got {symbols})");
        assert_eq!(stats.indexed_files, files,
            "stats.indexed_files must match count_indexed_files");
    }
}
