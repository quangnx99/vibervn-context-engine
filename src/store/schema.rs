/// SurrealDB DDL — applied at every startup to bootstrap or migrate tables, fields, and indexes.
///
/// Field definitions use `DEFINE FIELD OVERWRITE` so that re-running this DDL on an
/// EXISTING database actively updates the persisted field type. Without OVERWRITE, a
/// plain `DEFINE FIELD` is a no-op against a field that already exists — the old type
/// stays in the datastore and new writes that use the corrected type are rejected with
/// a FieldCheck type-violation error, rolling back the entire transaction. OVERWRITE
/// ensures the on-disk definition is always replaced with the current declaration.
///
/// Table definitions use `IF NOT EXISTS` to avoid recreating (and potentially
/// misconfiguring) tables that already hold data. Recreating a table is safe for
/// the schema but IF NOT EXISTS makes the intent explicit.
///
/// Index definitions use `IF NOT EXISTS` so that re-running the DDL on a live database
/// never triggers a rebuild of an existing index unnecessarily; a new index name will
/// still be created.
pub const SCHEMA_DDL: &str = r#"
-- SCHEMALESS: a SCHEMAFULL symbol table enforces per-field types. The native
-- sql::Array INSERT path writes `parent` as a plain string ("symbol:⟨fqn⟩"), which
-- an older SCHEMAFULL definition of `parent` as option<record<symbol>> rejects —
-- silently rolling back the ENTIRE batch (0 symbols persisted, no surfaced error).
-- SCHEMALESS removes that enforcement; field type safety is guaranteed at write time
-- by flush_symbol_batch_native (explicit Value types per field). Per-field validation
-- also cost ~4s for 27K rows during full rebuild.
--
-- OVERWRITE (not IF NOT EXISTS) + REMOVE FIELD runs on EVERY open_db, so an existing
-- SCHEMAFULL symbol table from a pre-v4 DB is flipped to SCHEMALESS synchronously
-- BEFORE the indexer writes — avoiding the race where the background v3→v4 migration
-- has not yet completed. Verified: OVERWRITE on a populated table preserves all rows
-- and is idempotent; REMOVE FIELD IF EXISTS is a no-op once the fields are gone.
DEFINE TABLE OVERWRITE symbol SCHEMALESS;
REMOVE FIELD IF EXISTS name       ON symbol;
REMOVE FIELD IF EXISTS kind       ON symbol;
REMOVE FIELD IF EXISTS file       ON symbol;
REMOVE FIELD IF EXISTS line_start ON symbol;
REMOVE FIELD IF EXISTS line_end   ON symbol;
REMOVE FIELD IF EXISTS signature  ON symbol;
REMOVE FIELD IF EXISTS parent     ON symbol;
DEFINE INDEX IF NOT EXISTS idx_symbol_file ON symbol FIELDS file;
DEFINE INDEX IF NOT EXISTS idx_symbol_name ON symbol FIELDS name;

-- SCHEMALESS: per-element array<float> validation on `embedding` costs ~530ms/95-chunk
-- insert (SurrealDB 2.x). Removing SCHEMAFULL drops this to ~83ms (8.9×). Field type
-- safety is enforced by Rust's ChunkRecord struct on the write path.
--
-- LANDMINE: SurrealDB v2 silently stores [] for f32 arrays under TYPE array (untyped)
-- and TYPE any. NEVER re-add a typed embedding field definition to this table.
DEFINE TABLE IF NOT EXISTS chunk SCHEMALESS;
DEFINE INDEX IF NOT EXISTS idx_chunk_file ON chunk FIELDS file;

-- calls was historically a graph RELATION (TYPE RELATION IN symbol OUT symbol).
-- At schema v5 NOTHING traverses the graph: every read path (query_callers/
-- query_callees in graph_expand.rs, call_graph in ops.rs) reads the denormalized
-- in_name/out_name/in_file/out_file columns via their secondary indexes. The
-- RELATION type forced SurrealDB to write graph-adjacency keys (->calls->,
-- <-calls<-) on EVERY edge insert — pure write amplification. On the Linux kernel
-- (4.44M edges) this measured ~44% of Phase-2 edge-write time. Flipping to a plain
-- NORMAL table eliminates that adjacency maintenance with byte-identical query
-- output (verified: call-graph node/edge digest unchanged). The v5→v6 migration
-- (run_migration_v5_to_v6) clears old RELATION rows so they are re-resolved as
-- plain rows. OVERWRITE (not IF NOT EXISTS) so an existing RELATION table is
-- flipped to NORMAL synchronously on open, before any edge write.
DEFINE TABLE OVERWRITE calls TYPE NORMAL SCHEMALESS;
DEFINE FIELD OVERWRITE line     ON calls TYPE int;
DEFINE FIELD OVERWRITE in_file  ON calls TYPE string;
DEFINE FIELD OVERWRITE out_file ON calls TYPE string;
DEFINE FIELD OVERWRITE in_name  ON calls TYPE option<string>;
DEFINE FIELD OVERWRITE out_name ON calls TYPE option<string>;
DEFINE INDEX IF NOT EXISTS idx_calls_in_file  ON calls FIELDS in_file;
DEFINE INDEX IF NOT EXISTS idx_calls_out_file ON calls FIELDS out_file;
DEFINE INDEX IF NOT EXISTS idx_calls_in_name  ON calls FIELDS in_name;
DEFINE INDEX IF NOT EXISTS idx_calls_out_name ON calls FIELDS out_name;

DEFINE TABLE IF NOT EXISTS uses TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD OVERWRITE in_file  ON uses TYPE string;
DEFINE FIELD OVERWRITE out_file ON uses TYPE string;
DEFINE INDEX IF NOT EXISTS idx_uses_in_file ON uses FIELDS in_file;
DEFINE INDEX IF NOT EXISTS idx_uses_out_file ON uses FIELDS out_file;

DEFINE TABLE IF NOT EXISTS imports TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD OVERWRITE in_file  ON imports TYPE string;
DEFINE FIELD OVERWRITE out_file ON imports TYPE string;
DEFINE INDEX IF NOT EXISTS idx_imports_in_file ON imports FIELDS in_file;
DEFINE INDEX IF NOT EXISTS idx_imports_out_file ON imports FIELDS out_file;

DEFINE TABLE IF NOT EXISTS contains TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD OVERWRITE in_file  ON contains TYPE string;
DEFINE FIELD OVERWRITE out_file ON contains TYPE string;
DEFINE INDEX IF NOT EXISTS idx_contains_in_file ON contains FIELDS in_file;
DEFINE INDEX IF NOT EXISTS idx_contains_out_file ON contains FIELDS out_file;

DEFINE TABLE IF NOT EXISTS implements TYPE RELATION IN symbol OUT symbol;
DEFINE FIELD OVERWRITE in_file  ON implements TYPE string;
DEFINE FIELD OVERWRITE out_file ON implements TYPE string;
DEFINE INDEX IF NOT EXISTS idx_implements_in_file ON implements FIELDS in_file;
DEFINE INDEX IF NOT EXISTS idx_implements_out_file ON implements FIELDS out_file;

DEFINE TABLE IF NOT EXISTS file_meta SCHEMAFULL;
DEFINE FIELD OVERWRITE path        ON file_meta TYPE string;
DEFINE FIELD OVERWRITE mtime       ON file_meta TYPE int;
DEFINE FIELD OVERWRITE size        ON file_meta TYPE int;
DEFINE FIELD OVERWRITE repo        ON file_meta TYPE string;
DEFINE FIELD OVERWRITE chunk_count ON file_meta TYPE int;
-- chunker_version: the cAST chunk-algorithm version that produced this file's
-- chunks. DEFAULT 0 so pre-existing rows (written before this field) read back
-- as 0, which never matches the current CHUNKER_VERSION (>= 1) → those files are
-- treated as modified by detect_changes and lazily re-chunked. No DB_SCHEMA_VERSION
-- bump: this is a freshness marker, not a data migration.
DEFINE FIELD OVERWRITE chunker_version ON file_meta TYPE int DEFAULT 0;
DEFINE INDEX IF NOT EXISTS idx_filemeta_path ON file_meta FIELDS path UNIQUE;

DEFINE TABLE IF NOT EXISTS index_meta SCHEMAFULL;
DEFINE FIELD OVERWRITE key   ON index_meta TYPE string;
DEFINE FIELD OVERWRITE value ON index_meta TYPE string;
DEFINE INDEX IF NOT EXISTS idx_meta_key ON index_meta FIELDS key UNIQUE;

DEFINE TABLE IF NOT EXISTS raw_edge SCHEMAFULL;
DEFINE FIELD OVERWRITE from_file    ON raw_edge TYPE string;
DEFINE FIELD OVERWRITE from_name    ON raw_edge TYPE string;
DEFINE FIELD OVERWRITE from_fqn     ON raw_edge TYPE string;
DEFINE FIELD OVERWRITE to_name      ON raw_edge TYPE string;
DEFINE FIELD OVERWRITE kind         ON raw_edge TYPE string;
DEFINE FIELD OVERWRITE line         ON raw_edge TYPE int;
DEFINE FIELD OVERWRITE import_path  ON raw_edge TYPE option<string>;
DEFINE INDEX IF NOT EXISTS idx_raw_edge_from_file ON raw_edge FIELDS from_file;
"#;
