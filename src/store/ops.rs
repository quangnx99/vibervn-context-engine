use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::local::Db;

use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};
use crate::parsing::relations::EdgeKind;
use crate::parsing::chunker::Chunk;

// ─── Null-tolerant integer deserializer ───────────────────────────────────
//
// SurrealDB SCHEMAFULL tables can return present-but-null values for integer
// fields when a row was inserted before the field was added to the schema
// (pre-v2 databases have file_meta rows without chunk_count). In that case
// the SELECT projection returns the key with a null value, which serde's
// plain i64 decoder rejects. The `#[serde(default)]` attribute only handles
// *absent* keys (not present-null), so we need a custom deserializer.
//
// This helper deserializes as `Option<i64>` and maps None (absent OR null)
// to 0. Combined with `#[serde(default)]` it covers all three cases:
//   - absent key:        `default` kicks in (returns 0) without calling this fn
//   - present-null:      this fn receives null, returns Ok(0)
//   - present integer:   this fn receives the integer, returns Ok(value)
fn de_null_as_zero_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<i64>::deserialize(deserializer)?.unwrap_or(0))
}

// ─── FileMeta ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub path: String,
    /// mtime is always written by the current pipeline but could theoretically
    /// be null in a corrupted or manually-created row. Null → 0.
    #[serde(default, deserialize_with = "de_null_as_zero_i64")]
    pub mtime: i64,
    /// size is always written by the current pipeline but could theoretically
    /// be null in a corrupted or manually-created row. Null → 0.
    #[serde(default, deserialize_with = "de_null_as_zero_i64")]
    pub size: i64,
    pub repo: String,
    /// Number of chunks indexed for this file. Added in DB schema v2.
    /// Pre-v2 rows have no value for this field; when explicitly projected
    /// in a SELECT, SurrealDB returns it as present-null rather than absent,
    /// so plain `#[serde(default)]` is insufficient. The combined
    /// `default, deserialize_with` form handles all three cases:
    ///   absent key → 0 (via `default`)
    ///   present-null → 0 (via `de_null_as_zero_i64`)
    ///   real integer → that value
    #[serde(default, deserialize_with = "de_null_as_zero_i64")]
    pub chunk_count: i64,
}

// ─── IndexMeta ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMeta {
    pub key: String,
    pub value: String,
}

// ─── DB row types for queries ─────────────────────────────────────────────

pub fn kind_to_str(k: &SymbolKind) -> &'static str {
    match k {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Trait => "trait",
        SymbolKind::Impl => "impl",
        SymbolKind::Class => "class",
        SymbolKind::Module => "module",
        SymbolKind::Interface => "interface",
    }
}

// ─── Delete operations (used in transactions) ────────────────────────────

/// Delete all edges, symbols, chunks, and file_meta for a given file path.
/// Edge deletion happens first (while symbol IDs still exist for traversal).
pub async fn delete_file_data(db: &Surreal<Db>, file_path: &str) -> Result<()> {
    // 1. Delete edges first (all relation tables by in_file or out_file).
    let path = file_path.to_string();

    db.query("DELETE FROM calls WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete calls")?;

    db.query("DELETE FROM uses WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete uses")?;

    db.query("DELETE FROM imports WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete imports")?;

    db.query("DELETE FROM contains WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete contains")?;

    db.query("DELETE FROM implements WHERE in_file = $path OR out_file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete implements")?;

    // 2. Delete symbols.
    db.query("DELETE FROM symbol WHERE file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete symbols")?;

    // 3. Delete chunks.
    db.query("DELETE FROM chunk WHERE file = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete chunks")?;

    // 4. Delete file_meta.
    db.query("DELETE FROM file_meta WHERE path = $path")
        .bind(("path", path.clone()))
        .await
        .context("delete file_meta")?;

    Ok(())
}

/// Bulk-delete all data for a set of file paths (edges, symbols, chunks, file_meta).
///
/// This replaces a per-file loop of `delete_file_data` calls, reducing O(files)
/// round-trips to O(tables) round-trips via `WHERE field IN $paths`.
pub async fn delete_files_data_bulk(db: &Surreal<Db>, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    // Edges first (all 5 relation tables, both directions).
    db.query("DELETE FROM calls WHERE in_file IN $paths OR out_file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete calls")?;

    db.query("DELETE FROM uses WHERE in_file IN $paths OR out_file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete uses")?;

    db.query("DELETE FROM imports WHERE in_file IN $paths OR out_file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete imports")?;

    db.query("DELETE FROM contains WHERE in_file IN $paths OR out_file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete contains")?;

    db.query("DELETE FROM implements WHERE in_file IN $paths OR out_file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete implements")?;

    // Symbols.
    db.query("DELETE FROM symbol WHERE file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete symbols")?;

    // Chunks.
    db.query("DELETE FROM chunk WHERE file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete chunks")?;

    // Raw edge staging rows for affected files.
    db.query("DELETE FROM raw_edge WHERE from_file IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete raw_edge")?;

    // file_meta.
    db.query("DELETE FROM file_meta WHERE path IN $paths")
        .bind(("paths", paths.to_vec()))
        .await
        .context("bulk delete file_meta")?;

    Ok(())
}

/// Delete ALL data — used for full rebuild.
pub async fn delete_all_data(db: &Surreal<Db>) -> Result<()> {
    // Edges first.
    db.query("DELETE FROM calls").await.context("delete all calls")?;
    db.query("DELETE FROM uses").await.context("delete all uses")?;
    db.query("DELETE FROM imports").await.context("delete all imports")?;
    db.query("DELETE FROM contains").await.context("delete all contains")?;
    db.query("DELETE FROM implements").await.context("delete all implements")?;
    // Raw edges staging table.
    db.query("DELETE FROM raw_edge").await.context("delete all raw_edge")?;
    // Then symbols, chunks, file_meta.
    db.query("DELETE FROM symbol").await.context("delete all symbols")?;
    db.query("DELETE FROM chunk").await.context("delete all chunks")?;
    db.query("DELETE FROM file_meta").await.context("delete all file_meta")?;
    Ok(())
}

// ─── Insert operations ────────────────────────────────────────────────────

/// Upsert a symbol using its deterministic record ID.
pub async fn upsert_symbol(db: &Surreal<Db>, sym: &Symbol) -> Result<()> {
    let record_id = sym.qualified.record_id();
    let kind_str = kind_to_str(&sym.kind);
    let parent_id = sym.parent_fqn.as_ref().map(|fqn| {
        format!("symbol:⟨{}⟩", fqn)
    });

    db.query(
        "UPSERT type::thing($id) SET \
         name = $name, kind = $kind, file = $file, \
         line_start = $line_start, line_end = $line_end, \
         signature = $signature, parent = $parent",
    )
    .bind(("id", record_id))
    .bind(("name", sym.qualified.name.clone()))
    .bind(("kind", kind_str.to_string()))
    .bind(("file", sym.qualified.file.clone()))
    .bind(("line_start", sym.line_start as i64))
    .bind(("line_end", sym.line_end as i64))
    .bind(("signature", sym.signature.clone()))
    .bind(("parent", parent_id))
    .await
    .context("upsert symbol")?;

    Ok(())
}

/// Insert a resolved edge using RELATE.
pub async fn insert_edge(
    db: &Surreal<Db>,
    from: &QualifiedSymbol,
    to: &QualifiedSymbol,
    kind: &EdgeKind,
    line: u32,
) -> Result<()> {
    let from_id = from.record_id();
    let to_id = to.record_id();
    let in_file = from.file.clone();
    let out_file = to.file.clone();

    match kind {
        EdgeKind::Calls => {
            db.query(
                "RELATE type::thing($from)->calls->type::thing($to) \
                 SET line = $line, in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("line", line as i64))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert calls edge")?;
        }
        EdgeKind::Uses => {
            db.query(
                "RELATE type::thing($from)->uses->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert uses edge")?;
        }
        EdgeKind::Imports => {
            db.query(
                "RELATE type::thing($from)->imports->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert imports edge")?;
        }
        EdgeKind::Contains => {
            db.query(
                "RELATE type::thing($from)->contains->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert contains edge")?;
        }
        EdgeKind::Implements => {
            db.query(
                "RELATE type::thing($from)->implements->type::thing($to) \
                 SET in_file = $in_file, out_file = $out_file",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("in_file", in_file))
            .bind(("out_file", out_file))
            .await
            .context("insert implements edge")?;
        }
    }

    Ok(())
}

/// Insert a chunk with its embedding.
pub async fn insert_chunk(db: &Surreal<Db>, chunk: &Chunk, embedding: Vec<f32>) -> Result<()> {
    let symbol_ref = chunk.symbol_ref.as_ref().map(|fqn| format!("symbol:⟨{}⟩", fqn));

    db.query(
        "CREATE chunk SET \
         file = $file, line_start = $line_start, line_end = $line_end, \
         content = $content, embedding = $embedding, symbol_ref = $symbol_ref",
    )
    .bind(("file", chunk.file.clone()))
    .bind(("line_start", chunk.line_start as i64))
    .bind(("line_end", chunk.line_end as i64))
    .bind(("content", chunk.content.clone()))
    .bind(("embedding", embedding))
    .bind(("symbol_ref", symbol_ref))
    .await
    .context("insert chunk")?;

    Ok(())
}

/// Upsert file metadata (including chunk_count).
pub async fn upsert_file_meta(db: &Surreal<Db>, meta: &FileMeta) -> Result<()> {
    db.query(
        "UPSERT file_meta SET path = $path, mtime = $mtime, size = $size, repo = $repo, \
         chunk_count = $chunk_count WHERE path = $path",
    )
    .bind(("path", meta.path.clone()))
    .bind(("mtime", meta.mtime))
    .bind(("size", meta.size))
    .bind(("repo", meta.repo.clone()))
    .bind(("chunk_count", meta.chunk_count))
    .await
    .context("upsert file_meta")?;

    Ok(())
}

// ─── Query operations ─────────────────────────────────────────────────────

/// Fetch all file_meta rows for a given repo.
pub async fn get_all_file_meta(db: &Surreal<Db>, repo: &str) -> Result<Vec<FileMeta>> {
    let rows: Vec<FileMeta> = db
        .query("SELECT path, mtime, size, repo, chunk_count FROM file_meta WHERE repo = $repo")
        .bind(("repo", repo.to_string()))
        .await
        .context("get all file_meta")?
        .take(0)?;
    Ok(rows)
}

/// Get a single index_meta value by key.
pub async fn get_meta(db: &Surreal<Db>, key: &str) -> Result<Option<String>> {
    let rows: Vec<IndexMeta> = db
        .query("SELECT key, value FROM index_meta WHERE key = $key")
        .bind(("key", key.to_string()))
        .await
        .context("get index_meta")?
        .take(0)?;
    Ok(rows.into_iter().next().map(|r| r.value))
}

/// Set an index_meta key/value.
pub async fn set_meta(db: &Surreal<Db>, key: &str, value: &str) -> Result<()> {
    db.query(
        "UPSERT index_meta SET key = $key, value = $value WHERE key = $key",
    )
    .bind(("key", key.to_string()))
    .bind(("value", value.to_string()))
    .await
    .context("set index_meta")?;
    Ok(())
}

/// Get all symbols from a given file (used for edge resolution).
pub async fn get_symbols_for_file(
    db: &Surreal<Db>,
    file: &str,
) -> Result<Vec<QualifiedSymbol>> {
    #[derive(Deserialize)]
    struct Row {
        file: String,
        name: String,
    }
    let rows: Vec<Row> = db
        .query("SELECT file, name FROM symbol WHERE file = $file")
        .bind(("file", file.to_string()))
        .await
        .context("get symbols for file")?
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

/// Find symbols by name across all files (for batched two-phase edge resolution).
///
/// Returns ONLY symbols whose name is in `names` — no full-table scan.
/// Relies on `idx_symbol_name` for efficient lookup.
pub async fn find_symbols_by_names(
    db: &Surreal<Db>,
    names: &[String],
) -> Result<Vec<QualifiedSymbol>> {
    if names.is_empty() {
        return Ok(vec![]);
    }

    #[derive(Deserialize)]
    struct Row {
        file: String,
        name: String,
    }

    let rows: Vec<Row> = db
        .query("SELECT file, name FROM symbol WHERE name IN $names")
        .bind(("names", names.to_vec()))
        .await
        .context("find_symbols_by_names")?
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

/// Symbol with positional info, for tie-break sorting in two-phase edge resolution.
#[derive(Debug, Clone)]
pub struct SymbolWithPos {
    /// Full FQN from `meta::id(id)` — used as the RELATE endpoint in Phase 2.
    pub fqn: String,
    pub file: String,
    pub name: String,
    pub line_start: i64,
    pub line_end: i64,
}

/// Find symbols by name, returning positional data needed for deterministic tie-breaking.
pub async fn find_symbols_by_names_with_pos(
    db: &Surreal<Db>,
    names: &[String],
) -> Result<Vec<SymbolWithPos>> {
    if names.is_empty() {
        return Ok(vec![]);
    }

    #[derive(Deserialize)]
    struct Row {
        fqn: String,
        file: String,
        name: String,
        line_start: i64,
        line_end: i64,
    }

    let rows: Vec<Row> = db
        .query("SELECT meta::id(id) AS fqn, file, name, line_start, line_end FROM symbol WHERE name IN $names")
        .bind(("names", names.to_vec()))
        .await
        .context("find_symbols_by_names_with_pos")?
        .take(0)?;

    Ok(rows
        .into_iter()
        .map(|r| SymbolWithPos {
            // meta::id(id) returns the ID portion with wrapping ⟨…⟩ for complex IDs
            // (e.g. "⟨/foo.rs::bar⟩"). Strip those brackets to recover the plain FQN
            // that was passed to QualifiedSymbol::fqn() at upsert time.
            fqn: strip_id_brackets(&r.fqn),
            file: r.file,
            name: r.name,
            line_start: r.line_start,
            line_end: r.line_end,
        })
        .collect())
}

/// Count indexed files for a repo.
pub async fn count_indexed_files(db: &Surreal<Db>, repo: &str) -> Result<u64> {
    #[derive(Deserialize)]
    struct Row {
        count: i64,
    }
    let rows: Vec<Row> = db
        .query("SELECT count() AS count FROM file_meta WHERE repo = $repo GROUP ALL")
        .bind(("repo", repo.to_string()))
        .await
        .context("count indexed files")?
        .take(0)?;
    Ok(rows.first().map(|r| r.count as u64).unwrap_or(0))
}

// ─── Index Explorer queries (read-only, bounded) ──────────────────────────
//
// Every helper below is capped with LIMIT or a count aggregate so a
// Linux-kernel-scale index never streams unbounded rows into the HTTP layer
// or the browser. Embeddings are reduced to their length server-side
// (`array::len`) so the float vectors never cross the wire.

#[derive(Deserialize)]
struct CountRow {
    count: i64,
}

/// Total chunk rows stored (whole DB — one DB per repo, so this is per-repo).
pub async fn count_chunks(db: &Surreal<Db>) -> Result<u64> {
    let rows: Vec<CountRow> = db
        .query("SELECT count() AS count FROM chunk GROUP ALL")
        .await
        .context("count chunks")?
        .take(0)?;
    Ok(rows.first().map(|r| r.count as u64).unwrap_or(0))
}

/// Total symbol rows stored.
pub async fn count_symbols(db: &Surreal<Db>) -> Result<u64> {
    let rows: Vec<CountRow> = db
        .query("SELECT count() AS count FROM symbol GROUP ALL")
        .await
        .context("count symbols")?
        .take(0)?;
    Ok(rows.first().map(|r| r.count as u64).unwrap_or(0))
}

/// Sample one stored embedding's dimensionality, or 0 if none embedded yet.
pub async fn sample_embedding_dim(db: &Surreal<Db>) -> Result<u64> {
    let rows: Vec<CountRow> = db
        .query(
            "SELECT array::len(embedding) AS count FROM chunk \
             WHERE embedding != [] LIMIT 1",
        )
        .await
        .context("sample embedding dim")?
        .take(0)?;
    Ok(rows.first().map(|r| r.count.max(0) as u64).unwrap_or(0))
}

/// One row in the file browser: path, language-agnostic metadata, and chunk count.
#[derive(Debug, Clone, Serialize)]
pub struct FileBrowserRow {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    pub chunks: u64,
}

/// Return a bounded, alphabetically-ordered page of indexed files for a repo,
/// each annotated with its chunk count. `limit` is hard-capped by the caller.
pub async fn files_page(
    db: &Surreal<Db>,
    repo: &str,
    limit: usize,
) -> Result<Vec<FileBrowserRow>> {
    #[derive(Deserialize)]
    struct MetaRow {
        path: String,
        /// Null-tolerant: pre-v2 rows may return null for explicitly-projected fields.
        #[serde(default, deserialize_with = "de_null_as_zero_i64")]
        mtime: i64,
        /// Null-tolerant: pre-v2 rows may return null for explicitly-projected fields.
        #[serde(default, deserialize_with = "de_null_as_zero_i64")]
        size: i64,
        /// Null-tolerant: pre-v2 rows have no chunk_count; SELECT returns present-null.
        #[serde(default, deserialize_with = "de_null_as_zero_i64")]
        chunk_count: i64,
    }

    let metas: Vec<MetaRow> = db
        .query(
            "SELECT path, mtime, size, chunk_count FROM file_meta \
             WHERE repo = $repo ORDER BY path LIMIT $limit",
        )
        .bind(("repo", repo.to_string()))
        .bind(("limit", limit as i64))
        .await
        .context("files_page: file_meta")?
        .take(0)?;

    Ok(metas
        .into_iter()
        .map(|m| FileBrowserRow {
            chunks: m.chunk_count.max(0) as u64,
            path: m.path,
            mtime: m.mtime,
            size: m.size,
        })
        .collect())
}

/// A chunk detail row (no embedding floats — only the dimension count).
#[derive(Debug, Clone, Serialize)]
pub struct ChunkDetailRow {
    pub line_start: i64,
    pub line_end: i64,
    pub content: String,
    pub embedding_dim: u64,
    pub symbol: Option<String>,
}

/// Return the chunks for a single file, ordered by line, bounded by `limit`.
pub async fn chunks_for_file(
    db: &Surreal<Db>,
    file: &str,
    limit: usize,
) -> Result<Vec<ChunkDetailRow>> {
    #[derive(Deserialize)]
    struct Row {
        line_start: i64,
        line_end: i64,
        content: String,
        embedding_dim: i64,
        symbol_ref: Option<String>,
    }
    let rows: Vec<Row> = db
        .query(
            "SELECT line_start, line_end, content, \
             array::len(embedding) AS embedding_dim, symbol_ref \
             FROM chunk WHERE file = $file ORDER BY line_start LIMIT $limit",
        )
        .bind(("file", file.to_string()))
        .bind(("limit", limit as i64))
        .await
        .context("chunks_for_file")?
        .take(0)?;

    Ok(rows
        .into_iter()
        .map(|r| ChunkDetailRow {
            line_start: r.line_start,
            line_end: r.line_end,
            content: r.content,
            embedding_dim: r.embedding_dim.max(0) as u64,
            symbol: r.symbol_ref.as_deref().and_then(strip_symbol_ref),
        })
        .collect())
}

/// A node in the call graph (one symbol).
#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line_start: i64,
    pub line_end: i64,
}

/// An edge in the call graph (caller → callee).
#[derive(Debug, Clone, Serialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
}

/// The call graph payload: nodes + edges, both bounded.
#[derive(Debug, Clone, Serialize)]
pub struct CallGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// True if the result was capped (more edges/symbols exist in the index).
    pub truncated: bool,
}

/// Build a bounded node-link view of the `calls` relation.
///
/// Strategy: take up to `edge_limit` call edges, collect the symbols they touch
/// (capped at `node_limit`), then emit the induced subgraph. Edges referencing a
/// symbol that was dropped by the node cap are themselves dropped, so the graph
/// is always internally consistent.
pub async fn call_graph(
    db: &Surreal<Db>,
    edge_limit: usize,
    node_limit: usize,
) -> Result<CallGraph> {
    // Symbol endpoints: in_name and out_name now store full FQNs (file::scope::name),
    // matching the node IDs produced by meta::id(id) below.
    #[derive(Deserialize)]
    struct EdgeRow {
        #[serde(rename = "in_name")]
        in_name: Option<String>,
        #[serde(rename = "out_name")]
        out_name: Option<String>,
    }

    let edge_rows: Vec<EdgeRow> = db
        .query(
            "SELECT in_name, out_name \
             FROM calls LIMIT $limit",
        )
        .bind(("limit", edge_limit as i64))
        .await
        .context("call_graph: edges")?
        .take(0)?;

    let total_edges = edge_rows.len();

    // Pull symbol metadata for nodes (bounded). Keyed by FQN from meta::id.
    #[derive(Deserialize)]
    struct SymRow {
        fqn: String,
        name: String,
        kind: String,
        file: String,
        line_start: i64,
        line_end: i64,
    }
    let sym_rows: Vec<SymRow> = db
        .query(
            "SELECT meta::id(id) AS fqn, name, kind, file, line_start, line_end FROM symbol LIMIT $limit",
        )
        .bind(("limit", node_limit as i64))
        .await
        .context("call_graph: symbols")?
        .take(0)?;

    let mut nodes: Vec<GraphNode> = Vec::with_capacity(sym_rows.len());
    let mut node_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in sym_rows {
        // meta::id(id) wraps complex IDs in ⟨…⟩ — strip them to get the plain FQN.
        let id = strip_id_brackets(&s.fqn);
        if node_ids.insert(id.clone()) {
            nodes.push(GraphNode {
                id,
                name: s.name,
                kind: s.kind,
                file: s.file,
                line_start: s.line_start,
                line_end: s.line_end,
            });
        }
    }

    let mut edges: Vec<GraphEdge> = Vec::new();
    for e in edge_rows {
        let (in_name, out_name) = match (e.in_name, e.out_name) {
            (Some(i), Some(o)) => (i, o),
            _ => continue,
        };
        // in_name and out_name now store full FQNs, matching node_ids keyed by FQN.
        let source = in_name;
        let target = out_name;
        if node_ids.contains(&source) && node_ids.contains(&target) {
            edges.push(GraphEdge { source, target });
        }
    }

    let truncated = total_edges >= edge_limit || nodes.len() >= node_limit;
    Ok(CallGraph { nodes, edges, truncated })
}

/// Strip the stored `symbol:⟨fqn⟩` wrapper and return just the symbol name.
fn strip_symbol_ref(s: &str) -> Option<String> {
    s.strip_prefix("symbol:⟨")
        .and_then(|s| s.strip_suffix("⟩"))
        .map(|fqn| fqn.rsplit("::").next().unwrap_or(fqn).to_string())
}

/// Strip the SurrealDB complex-ID wrapper `⟨…⟩` returned by `meta::id(id)`.
///
/// SurrealDB encodes record IDs that contain non-standard characters by wrapping
/// them in `⟨` / `⟩` angle brackets. When projected with `meta::id(id) AS fqn`,
/// a record whose ID is `symbol:⟨/foo.rs::bar⟩` returns `fqn = "⟨/foo.rs::bar⟩"`.
/// This helper strips those brackets to recover the plain FQN string.
fn strip_id_brackets(id: &str) -> String {
    id.strip_prefix("⟨")
        .and_then(|s| s.strip_suffix("⟩"))
        .unwrap_or(id)
        .to_string()
}

// ─── STEP 1 proof test ────────────────────────────────────────────────────
//
// Before the schema fix, `symbol_ref` and `parent` were declared as
// `option<record<symbol>>` (SCHEMAFULL). Assigning a quoted string to those
// fields caused SurrealDB to roll back the whole transaction silently —
// `db.query(&txn).await` returned Ok but the row was not committed.
//
// After the fix (both fields changed to `option<string>`), quoted strings
// commit correctly. These tests verify the fixed behaviour: the transaction
// MUST persist the row, and `.check()` MUST return Ok (no per-statement error).
#[cfg(test)]
mod silent_rollback_proof {
    use super::*;
    use crate::store::open_db;
    use tempfile::TempDir;

    /// Open a fresh in-memory-equivalent SurrealKv DB in a tempdir with the
    /// real schema applied, returning it ready for writes.
    async fn open_test_db(home: &TempDir, repo: &str) -> Surreal<surrealdb::engine::local::Db> {
        open_db(home.path(), repo).await.expect("open test db")
    }

    /// After the schema fix (`symbol_ref option<string>`), a quoted-string assignment
    /// MUST commit. The transaction must persist the chunk and `.check()` must be Ok.
    #[tokio::test]
    async fn string_assigned_to_symbol_ref_commits_after_schema_fix() {
        let home = TempDir::new().unwrap();
        let db = open_test_db(&home, "/test/proof_repo").await;

        // Mimics exactly what pipeline.rs writes for a chunk with a symbol_ref.
        let txn = "\
BEGIN TRANSACTION;\n\
CREATE chunk SET \
  file = '/test/foo.rs', \
  line_start = 1, \
  line_end = 5, \
  content = 'fn foo() {}', \
  embedding = [], \
  symbol_ref = 'symbol:⟨foo::bar⟩';\n\
COMMIT TRANSACTION;\n";

        // .await must not err.
        let resp = db.query(txn).await.expect(".await must not err after schema fix");

        // .check() must be Ok — no per-statement error (string is valid for option<string>).
        resp.check().expect(".check() must be Ok after schema fix: schema now accepts string");

        let chunk_count = count_chunks(&db).await.unwrap();
        println!("FIXED — chunk count with quoted symbol_ref: {chunk_count}");
        assert_eq!(
            chunk_count,
            1,
            "chunk must persist after schema fix (got {chunk_count})"
        );
    }

    /// Control test: a chunk with symbol_ref = NONE must still commit.
    #[tokio::test]
    async fn chunk_with_none_symbol_ref_commits_ok() {
        let home = TempDir::new().unwrap();
        let db = open_test_db(&home, "/test/control_repo").await;

        let txn = "\
BEGIN TRANSACTION;\n\
CREATE chunk SET \
  file = '/test/foo.rs', \
  line_start = 1, \
  line_end = 5, \
  content = 'fn foo() {}', \
  embedding = [], \
  symbol_ref = NONE;\n\
COMMIT TRANSACTION;\n";

        db.query(txn).await.expect("txn must not err");
        let count = count_chunks(&db).await.unwrap();
        println!("NONE symbol_ref — chunk count: {count}");
        assert_eq!(count, 1, "chunk with NONE symbol_ref must persist (got {count})");
    }

    /// After the schema fix (`parent option<string>`), a quoted-string assignment
    /// to `parent` MUST commit.
    #[tokio::test]
    async fn string_assigned_to_parent_field_commits_after_schema_fix() {
        let home = TempDir::new().unwrap();
        let db = open_test_db(&home, "/test/parent_proof_repo").await;

        // Mirrors what append_upsert_symbol writes when parent_fqn is Some.
        let txn = "\
BEGIN TRANSACTION;\n\
UPSERT symbol:`⟨/test/foo.rs::/test/foo.rs::bar⟩` SET \
  name = 'bar', \
  kind = 'function', \
  file = '/test/foo.rs', \
  line_start = 1, \
  line_end = 5, \
  signature = NONE, \
  parent = 'symbol:⟨/test/foo.rs::/test/foo.rs::outer⟩';\n\
COMMIT TRANSACTION;\n";

        let resp = db.query(txn).await.expect(".await must not err after schema fix");
        resp.check().expect(".check() must be Ok: parent is now option<string>");

        let count = count_symbols(&db).await.unwrap();
        println!("FIXED — symbol count with quoted parent: {count}");
        assert_eq!(
            count,
            1,
            "symbol must persist after schema fix (got {count})"
        );
    }
}


// Null chunk_count deserialization tests
//
// Regression test for the crash:
//   "Serialization error: failed to deserialize; expected a 64-bit signed
//    integer, found None"
//
// Root cause: pre-v2 on-disk databases have file_meta rows with no chunk_count
// value. get_all_file_meta and files_page explicitly project chunk_count in
// their SELECT; SurrealDB returns the key *present with a null value* (not
// absent). The old plain i64 field rejected null -> crash. The new
// #[serde(default, deserialize_with = "de_null_as_zero_i64")] handles all
// three cases: absent key, present-null, and real integer.
//
// On testing the DB-level scenario:
//   The exact production scenario (pre-v2 row with absent chunk_count, re-read
//   after full DDL is applied) cannot be reliably reproduced in unit tests
//   because SurrealDB embedded (SurrealKv) does not support two separate
//   Surreal::new handles on the same on-disk path -- the second open fails with
//   "Invalid revision 0 for type DefineTableStatement". This is a known
//   SurrealDB embedded engine constraint (same issue documented in the
//   isolation_repro test in store/mod.rs).
//
//   Instead we:
//     1. Unit-test the serde logic directly via JSON (covers the null/absent
//        decoding that IS the root cause of the crash).
//     2. Integration-test that a normally-written row round-trips correctly
//        through open_db (ensures the fix does not break the happy path).
#[cfg(test)]
mod null_chunk_count_deserialization {
    use super::*;
    use crate::store::open_db;
    use tempfile::TempDir;

    // Serde unit tests: these directly exercise the de_null_as_zero_i64
    // helper and are the authoritative proof that the deserialization fix is
    // correct. The JSON payloads mirror exactly what SurrealDB returns when
    // chunk_count is null (pre-v2 row) or absent (truly missing key).

    /// FileMeta with chunk_count = null (present-null) must decode as 0.
    /// Without the fix, serde rejects null -> i64 with the exact crash error.
    #[test]
    fn file_meta_null_chunk_count_decodes_as_zero() {
        let json = r#"{"path":"/a.rs","mtime":100,"size":200,"repo":"/repo","chunk_count":null}"#;
        let meta: FileMeta = serde_json::from_str(json)
            .expect("FileMeta with null chunk_count must deserialize without error");
        assert_eq!(meta.chunk_count, 0, "null chunk_count must become 0");
        assert_eq!(meta.mtime, 100);
        assert_eq!(meta.size, 200);
    }

    /// FileMeta with chunk_count absent (missing key) must decode as 0.
    /// Covered by #[serde(default)].
    #[test]
    fn file_meta_absent_chunk_count_decodes_as_zero() {
        let json = r#"{"path":"/a.rs","mtime":100,"size":200,"repo":"/repo"}"#;
        let meta: FileMeta = serde_json::from_str(json)
            .expect("FileMeta with absent chunk_count must deserialize without error");
        assert_eq!(meta.chunk_count, 0, "absent chunk_count must become 0");
    }

    /// FileMeta with a real chunk_count value must decode as that value.
    #[test]
    fn file_meta_real_chunk_count_decodes_correctly() {
        let json = r#"{"path":"/a.rs","mtime":100,"size":200,"repo":"/repo","chunk_count":42}"#;
        let meta: FileMeta = serde_json::from_str(json)
            .expect("FileMeta with real chunk_count must deserialize");
        assert_eq!(meta.chunk_count, 42);
    }

    /// mtime = null must decode as 0 (defensive null-tolerance for mtime).
    #[test]
    fn file_meta_null_mtime_decodes_as_zero() {
        let json = r#"{"path":"/a.rs","mtime":null,"size":200,"repo":"/repo","chunk_count":0}"#;
        let meta: FileMeta = serde_json::from_str(json)
            .expect("FileMeta with null mtime must deserialize without error");
        assert_eq!(meta.mtime, 0, "null mtime must become 0");
    }

    /// size = null must decode as 0 (defensive null-tolerance for size).
    #[test]
    fn file_meta_null_size_decodes_as_zero() {
        let json = r#"{"path":"/a.rs","mtime":100,"size":null,"repo":"/repo","chunk_count":0}"#;
        let meta: FileMeta = serde_json::from_str(json)
            .expect("FileMeta with null size must deserialize without error");
        assert_eq!(meta.size, 0, "null size must become 0");
    }

    // Integration test: post-v2 happy path

    /// A row written via upsert_file_meta (with a real chunk_count) must round-trip
    /// correctly through get_all_file_meta. This ensures the fix does not break the
    /// normal post-v2 read path.
    #[tokio::test]
    async fn get_all_file_meta_decodes_real_chunk_count() {
        let home = TempDir::new().unwrap();
        let db = open_db(home.path(), "/repo/real_chunk_count_test")
            .await
            .expect("open_db");

        let meta = FileMeta {
            path: "/repo/b.rs".to_string(),
            mtime: 999,
            size: 4096,
            repo: "/repo/real_chunk_count_test".to_string(),
            chunk_count: 42,
        };
        upsert_file_meta(&db, &meta).await.expect("upsert");

        let rows = get_all_file_meta(&db, "/repo/real_chunk_count_test")
            .await
            .expect("get_all_file_meta");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chunk_count, 42, "real chunk_count must round-trip correctly");
        assert_eq!(rows[0].mtime, 999);
        assert_eq!(rows[0].size, 4096);
    }
}
