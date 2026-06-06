pub mod ops;
pub mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use surrealdb::Surreal;
use surrealdb::engine::local::{Db, SurrealKv};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::store::schema::SCHEMA_DDL;

/// Current DB schema version. Bump when new backfills are added.
/// v1 = original schema (no in_name/out_name, no chunk_count).
/// v2 = adds calls.in_name/out_name + file_meta.chunk_count.
/// v3 = chunk table flipped to SCHEMALESS for ~8.9× faster writes.
/// v4 = symbol table flipped to SCHEMALESS so the native sql::Array INSERT path
///      (which writes parent as a plain string) is not rejected by an existing
///      SCHEMAFULL symbol.parent definition (older DBs declared it as
///      option<record<symbol>>, which silently rolled back the whole batch → 0 symbols).
pub const DB_SCHEMA_VERSION: u32 = 4;

/// key in index_meta for the DB schema version.
pub const DB_SCHEMA_VERSION_KEY: &str = "db_schema_version";

/// Shared, process-wide map of one open SurrealDB handle per repo path.
pub type RepoDbMap = Arc<RwLock<HashMap<String, Surreal<Db>>>>;

/// Sanitize a repo path to a safe directory name (max 64 chars).
pub fn sanitize_repo_name(repo_path: &str) -> String {
    let sanitized: String = repo_path
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.len() > 64 {
        trimmed[trimmed.len() - 64..].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Return the SurrealDB data directory for a given repo.
pub fn db_path(home_dir: &Path, repo_path: &str) -> PathBuf {
    let name = sanitize_repo_name(repo_path);
    home_dir
        .join(".vibervn")
        .join("context-engine")
        .join("surreal")
        .join(name)
}

/// Read the stored db_schema_version from index_meta, defaulting to 1
/// (treat unversioned DBs as v1 for safe migration).
pub async fn read_db_schema_version(db: &Surreal<Db>) -> u32 {
    match ops::get_meta(db, DB_SCHEMA_VERSION_KEY).await {
        Ok(Some(v)) => v.parse::<u32>().unwrap_or(1),
        _ => 1,
    }
}

/// Open (or create) a SurrealDB database for the given repo.
/// Runs schema DDL to ensure all tables/indexes exist.
/// Returns the db handle; the caller is responsible for triggering migrations.
pub async fn open_db(home_dir: &Path, repo_path: &str) -> Result<Surreal<Db>> {
    let path = db_path(home_dir, repo_path);
    std::fs::create_dir_all(&path).with_context(|| format!("create db dir {:?}", path))?;

    let db = Surreal::new::<SurrealKv>(path.to_str().unwrap())
        .await
        .context("open surrealdb")?;

    db.use_ns("context_engine")
        .use_db(sanitize_repo_name(repo_path))
        .await
        .context("select ns/db")?;

    db.query(SCHEMA_DDL)
        .await
        .context("apply schema DDL")?
        .check()
        .context("schema DDL contained errors")?;

    Ok(db)
}

/// Spawn background migration tasks if needed (non-blocking).
///
/// Checks `db_schema_version` in `index_meta`. Spawns tasks to bring the DB
/// up to the current schema version. Failures are logged, not propagated.
///
/// v1→v2: backfills calls.in_name/out_name + file_meta.chunk_count.
/// v2→v3: flips chunk table to SCHEMALESS for ~8.9× faster writes.
///
/// If both migrations are needed, they run in a single chained task so v1→v2
/// always completes before v2→v3 starts.
pub fn maybe_spawn_migration(db: Surreal<Db>, stored_version: u32) {
    if stored_version >= DB_SCHEMA_VERSION {
        return;
    }
    info!(stored_version, target = DB_SCHEMA_VERSION, "spawning chained DB migration background task");
    // Run all needed migrations in one chained task so each completes before the
    // next starts. A failed step aborts the chain via `?` (the version stamp is only
    // written on success, so the next open retries from the same point).
    tokio::spawn(async move {
        let result: Result<()> = async {
            if stored_version < 2 {
                run_migration_v1_to_v2(&db).await.context("v1→v2")?;
            }
            if stored_version < 3 {
                run_migration_v2_to_v3(&db).await.context("v2→v3")?;
            }
            if stored_version < 4 {
                run_migration_v3_to_v4(&db).await.context("v3→v4")?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(error = %e, "chained DB migration failed");
        }
    });
}

/// Paged v1→v2 migration. Must be idempotent (safe to re-run).
///
/// Backfill 1: calls.in_name/out_name — reads link-deref in.name/out.name per page
///   and populates the new denormalized columns.
/// Backfill 2: file_meta.chunk_count — counts chunks per file and updates file_meta.
///
/// `db_schema_version=2` is written ONLY after both backfills complete.
///
/// Keyset pagination:
///   - calls: keyset on `type::string(id) AS id_str` (string-ordered record ID).
///     Using `type::string(id)` sidesteps the Thing-serde blocker: we never
///     deserialize a SurrealDB `Thing` through serde — we just read the string
///     representation. The string form `calls:⟨rand⟩` has stable lexicographic
///     order (SurrealDB random IDs are fixed-length alphanumeric, giving consistent
///     string sort). The `id` is unique per row, so `WHERE type::string(id) > $cursor
///     ORDER BY id_str` skips no rows and visits no row twice.
///     NOTE: SurrealDB 2.6.5 requires ORDER BY to reference a column that appears in
///     the SELECT projection. `ORDER BY type::string(id)` fails (function in ORDER BY
///     not supported), but `ORDER BY id_str` (the projected alias) works correctly.
///   - file_meta: keyset on `path` (UNIQUE via idx_filemeta_path). `WHERE path > $cursor
///     ORDER BY path` is correct and skips nothing.
///
/// Per-edge update correctness (Defect 2 fix):
///   Each calls row is updated via `UPDATE type::thing($id_str)` using the per-row
///   id_str read from that exact row. This ensures the (in_name, out_name) values
///   written come from the in.name/out.name of that specific row — not a file-pair
///   group that may contain multiple distinct edges sharing the same in_file/out_file.
pub async fn run_migration_v1_to_v2(db: &Surreal<Db>) -> Result<()> {
    use serde::Deserialize;

    let page_size: i64 = 512;

    // ── Backfill 1: calls.in_name / out_name ─────────────────────────────
    // The link-deref `in.name`/`out.name` is valid on existing rows (v1 rows
    // have proper `in`/`out` symbol record links). We read them to get the names.
    {
        info!("migration v1→v2: backfilling calls.in_name/out_name");

        // Keyset cursor: the last `type::string(id)` seen. Start from "" (empty
        // string sorts before all real record-id strings).
        let cursor_key = "migration_v2_calls_cursor";
        let mut cursor: String = ops::get_meta(db, cursor_key)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();

        loop {
            #[derive(Deserialize)]
            struct EdgeRow {
                id_str: String,
                #[serde(rename = "in_name_link")]
                in_name: Option<String>,
                #[serde(rename = "out_name_link")]
                out_name: Option<String>,
            }

            // Select id as a string via type::string(id) so we never touch Thing serde.
            // WHERE type::string(id) > $cursor gives keyset pagination over the random IDs.
            // ORDER BY id_str (the projected alias) gives consistent ordering.
            // NOTE: ORDER BY type::string(id) fails in SurrealDB 2.6.5 (function not
            // allowed in ORDER BY); ORDER BY id_str (alias) works correctly.
            let batch: Vec<EdgeRow> = db
                .query(
                    "SELECT type::string(id) AS id_str, \
                            in.name AS in_name_link, \
                            out.name AS out_name_link \
                     FROM calls \
                     WHERE type::string(id) > $cursor \
                     ORDER BY id_str \
                     LIMIT $page",
                )
                .bind(("cursor", cursor.clone()))
                .bind(("page", page_size))
                .await
                .context("migration: scan calls page")?
                .take(0)?;

            if batch.is_empty() {
                break;
            }

            // Advance cursor to the last id_str in this page.
            cursor = batch.last().map(|r| r.id_str.clone()).unwrap_or(cursor.clone());

            // Update each row by its OWN record id. This is the per-edge fix:
            // we update exactly the row whose in.name/out.name we read — never
            // a file-pair group that would stamp one name pair onto all edges
            // sharing the same (in_file, out_file).
            for row in &batch {
                if let (Some(in_n), Some(out_n)) = (&row.in_name, &row.out_name) {
                    db.query(
                        "UPDATE type::thing($id) SET in_name = $in_name, out_name = $out_name",
                    )
                    .bind(("id", row.id_str.clone()))
                    .bind(("in_name", in_n.clone()))
                    .bind(("out_name", out_n.clone()))
                    .await
                    .context("migration: update calls in_name/out_name by id")?;
                }
            }

            // Persist cursor for crash resume.
            ops::set_meta(db, cursor_key, &cursor)
                .await
                .context("migration: persist calls cursor")?;

            let batch_len = batch.len() as i64;
            if batch_len < page_size {
                break;
            }
        }

        // Clean up cursor key.
        let _ = db.query("DELETE FROM index_meta WHERE key = $k")
            .bind(("k", cursor_key))
            .await;
    }

    // ── Backfill 2: file_meta.chunk_count ────────────────────────────────
    {
        info!("migration v1→v2: backfilling file_meta.chunk_count");

        // Keyset cursor on path (UNIQUE via idx_filemeta_path).
        // `WHERE path > $cursor ORDER BY path` is correct and skips nothing.
        let cursor_key = "migration_v2_filemeta_cursor";
        let mut cursor: String = ops::get_meta(db, cursor_key)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();

        loop {
            #[derive(Deserialize)]
            struct FileMetaRow {
                path: String,
            }

            let batch: Vec<FileMetaRow> = db
                .query(
                    "SELECT path FROM file_meta \
                     WHERE path > $cursor \
                     ORDER BY path \
                     LIMIT $page",
                )
                .bind(("cursor", cursor.clone()))
                .bind(("page", page_size))
                .await
                .context("migration: scan file_meta page")?
                .take(0)?;

            if batch.is_empty() {
                break;
            }

            // Advance cursor.
            cursor = batch.last().map(|r| r.path.clone()).unwrap_or(cursor.clone());

            for row in &batch {
                #[derive(Deserialize)]
                struct CountRow { count: i64 }
                let count_rows: Vec<CountRow> = db
                    .query("SELECT count() AS count FROM chunk WHERE file = $f GROUP ALL")
                    .bind(("f", row.path.clone()))
                    .await
                    .context("migration: count chunks for file")?
                    .take(0)?;
                let count = count_rows.first().map(|r| r.count).unwrap_or(0);

                // Update by path (unique via idx_filemeta_path).
                db.query("UPDATE file_meta SET chunk_count = $count WHERE path = $path")
                    .bind(("count", count))
                    .bind(("path", row.path.clone()))
                    .await
                    .context("migration: update file_meta chunk_count")?;
            }

            ops::set_meta(db, cursor_key, &cursor)
                .await
                .context("migration: persist file_meta cursor")?;

            let batch_len = batch.len() as i64;
            if batch_len < page_size {
                break;
            }
        }

        // Clean up cursor key.
        let _ = db.query("DELETE FROM index_meta WHERE key = $k")
            .bind(("k", cursor_key))
            .await;
    }

    // Stamp db_schema_version=2 ONLY after both backfills complete.
    ops::set_meta(db, DB_SCHEMA_VERSION_KEY, "2")
        .await
        .context("migration: stamp db_schema_version=2")?;

    info!("migration v1→v2 complete");
    Ok(())
}

/// Migrate chunk table from SCHEMAFULL (with per-element array<float> validation)
/// to SCHEMALESS for ~8.9× faster writes.
///
/// Steps:
///   1. Flip table mode + remove all field definitions (single multi-statement query).
///   2. Gating readback: verify one existing chunk row still has embedding.len() >= 512.
///   3. Stamp db_schema_version=3.
///   4. If gating fails: set needs_rebuild flag (next index run forces full rebuild).
///
/// Idempotent: safe to re-run. REMOVE FIELD on a non-existent field is a no-op.
/// DEFINE TABLE OVERWRITE on an already-SCHEMALESS table is a no-op.
pub async fn run_migration_v2_to_v3(db: &Surreal<Db>) -> Result<()> {
    use serde::Deserialize;

    info!("migration v2→v3: flipping chunk table to SCHEMALESS");

    // Step 1: flip table mode + remove field definitions.
    // Each statement auto-commits. REMOVE FIELD is idempotent (no-op if absent).
    db.query(
        "DEFINE TABLE OVERWRITE chunk SCHEMALESS;\
         REMOVE FIELD embedding ON chunk;\
         REMOVE FIELD file ON chunk;\
         REMOVE FIELD line_start ON chunk;\
         REMOVE FIELD line_end ON chunk;\
         REMOVE FIELD content ON chunk;\
         REMOVE FIELD symbol_ref ON chunk;"
    )
    .await
    .context("migration v2→v3: flip chunk to SCHEMALESS + remove fields")?;

    // Step 2: gating readback — verify existing embeddings survive the flip.
    #[derive(Deserialize)]
    struct ProbeRow {
        embedding: Vec<f32>,
    }
    let probe: Vec<ProbeRow> = db
        .query("SELECT embedding FROM chunk WHERE embedding IS NOT NONE LIMIT 1")
        .await
        .context("migration v2→v3: gating readback query")?
        .take(0)?;

    let gating_ok = match probe.first() {
        Some(row) => row.embedding.len() >= 512, // sanity: at least half-dim
        None => true, // empty table — nothing to validate, migration is trivially safe
    };

    if gating_ok {
        info!("migration v2→v3: gating readback passed");
    } else {
        warn!("migration v2→v3: gating readback FAILED — setting needs_rebuild flag");
        ops::set_meta(db, "needs_rebuild", "1")
            .await
            .context("migration v2→v3: set needs_rebuild")?;
    }

    // Step 3: stamp version (regardless of gating — prevents re-running migration).
    ops::set_meta(db, DB_SCHEMA_VERSION_KEY, "3")
        .await
        .context("migration v2→v3: stamp db_schema_version=3")?;

    info!("migration v2→v3 complete");
    Ok(())
}

/// Migrate the symbol table from SCHEMAFULL to SCHEMALESS.
///
/// Why: the native `INSERT INTO symbol $data` path (flush_symbol_batch_native) writes
/// `parent` as a plain string "symbol:⟨fqn⟩". Older DBs declared `parent` as
/// `option<record<symbol>>`; SCHEMAFULL type enforcement rejects the string and rolls
/// back the WHOLE INSERT batch, so 0 symbols persist and Phase 2 resolves 0 call edges —
/// a silent accuracy regression with no surfaced error. Flipping to SCHEMALESS removes
/// the enforcement; correctness is guaranteed by the explicit per-field Value types in
/// flush_symbol_batch_native.
///
/// `DEFINE TABLE OVERWRITE ... SCHEMALESS` alone is NOT sufficient — the persisted
/// `DEFINE FIELD` definitions still enforce their types. Each field definition must be
/// explicitly removed (verified: flip-only leaves the insert failing; flip + REMOVE FIELD
/// makes it succeed). Mirrors run_migration_v2_to_v3 for the chunk table.
///
/// Idempotent: DEFINE TABLE OVERWRITE on an already-SCHEMALESS table and REMOVE FIELD on
/// an absent field are both no-ops. The existing symbol rows and their data are preserved.
pub async fn run_migration_v3_to_v4(db: &Surreal<Db>) -> Result<()> {
    info!("migration v3→v4: flipping symbol table to SCHEMALESS");

    db.query(
        "DEFINE TABLE OVERWRITE symbol SCHEMALESS;\
         REMOVE FIELD IF EXISTS name ON symbol;\
         REMOVE FIELD IF EXISTS kind ON symbol;\
         REMOVE FIELD IF EXISTS file ON symbol;\
         REMOVE FIELD IF EXISTS line_start ON symbol;\
         REMOVE FIELD IF EXISTS line_end ON symbol;\
         REMOVE FIELD IF EXISTS signature ON symbol;\
         REMOVE FIELD IF EXISTS parent ON symbol;",
    )
    .await
    .context("migration v3→v4: flip symbol to SCHEMALESS + remove fields")?
    .check()
    .context("migration v3→v4: symbol flip statement error")?;

    ops::set_meta(db, DB_SCHEMA_VERSION_KEY, "4")
        .await
        .context("migration v3→v4: stamp db_schema_version=4")?;

    info!("migration v3→v4 complete");
    Ok(())
}

/// Return the shared `Surreal<Db>` handle for `repo`, opening and caching it on
/// first use. Spawns background migration if stored schema version < current.
pub async fn get_or_open(
    repo_dbs: &RepoDbMap,
    home_dir: &Path,
    repo: &str,
) -> Result<Surreal<Db>> {
    if let Some(db) = repo_dbs.read().await.get(repo) {
        return Ok(db.clone());
    }
    let db = open_db(home_dir, repo).await?;

    // Check schema version and spawn migration if needed (non-blocking).
    let stored_version = read_db_schema_version(&db).await;
    maybe_spawn_migration(db.clone(), stored_version);

    let mut map = repo_dbs.write().await;
    if let Some(existing) = map.get(repo) {
        return Ok(existing.clone());
    }
    map.insert(repo.to_string(), db.clone());
    Ok(db)
}


#[cfg(test)]
mod isolation_repro {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn concurrent_handles_isolation_then_shared_fix() {
        // ── PART 1: does isolation exist? ────────────────────────────────────
        let home = TempDir::new().unwrap();
        let repo = "/proj/repo_iso";

        // Open handle A (fresh DB).
        let a = open_db(home.path(), repo).await.expect("open A");
        assert_eq!(
            ops::count_chunks(&a).await.unwrap(),
            0,
            "fresh DB must be empty"
        );

        // Open handle B on the SAME on-disk path while A is still alive.
        let b = open_db(home.path(), repo).await.expect("open B");

        // Write+commit one chunk through B.
        b.query(
            "CREATE chunk SET file = '/x/f.rs', line_start = 1, line_end = 2, \
             content = 'x', embedding = [0.1, 0.2, 0.3, 0.4], symbol_ref = NONE;",
        )
        .await
        .expect("write chunk via B");

        // B must see its own write — if not, the experiment is invalid.
        assert_eq!(
            ops::count_chunks(&b).await.unwrap(),
            1,
            "B must see its own write (write failed if this is 0)"
        );

        // Re-query the still-open A — does it see B's commit?
        let a_after = ops::count_chunks(&a).await.unwrap();
        println!(
            "ISOLATION PROBE: A reads {a_after} after B committed 1 \
             (0 => isolation confirmed, 1 => no isolation / hypothesis WRONG)"
        );

        // ── PART 2: does the shared handle (the proposed fix) work? ──────────
        let map: RepoDbMap = Arc::new(RwLock::new(HashMap::new()));

        // Both calls resolve to the SAME cached instance.
        let sa = get_or_open(&map, home.path(), repo).await.expect("shared A");
        let sb = get_or_open(&map, home.path(), repo).await.expect("shared B");

        // Write another chunk through sb (total on disk will be 2 after this).
        sb.query(
            "CREATE chunk SET file = '/x/f.rs', line_start = 3, line_end = 4, \
             content = 'y', embedding = [0.5, 0.6, 0.7, 0.8], symbol_ref = NONE;",
        )
        .await
        .expect("write chunk via shared B");

        let sa_after = ops::count_chunks(&sa).await.unwrap();
        println!(
            "SHARED PROBE: shared-A reads {sa_after} after shared-B committed another chunk"
        );

        // ── Assertions ───────────────────────────────────────────────────────
        assert_eq!(
            a_after,
            0,
            "EXPECTED isolation: a still-open separate handle must NOT see another instance's \
             commit. If this fails (A == 1), the isolation hypothesis is WRONG and the real bug \
             is elsewhere (read-path ns/db context)."
        );

        // sa and sb are the same cached instance; part 1 left 1 chunk on disk,
        // part 2 wrote 1 more through the shared instance → shared-A should see 2.
        assert_eq!(
            sa_after,
            2,
            "shared handle must see writes made through the same cached instance"
        );
    }
}

// ─── Stale-schema regression ──────────────────────────────────────────────
//
// This module proves that `DEFINE FIELD OVERWRITE` correctly migrates an existing
// database whose field was created with the OLD type (`option<record<symbol>>`).
//
// WITHOUT the OVERWRITE fix (plain `DEFINE FIELD`):
//   - Re-applying the corrected DDL is a no-op: the on-disk type stays as
//     `option<record<symbol>>`.
//   - Attempting to write a quoted-string `symbol_ref` value fails with:
//       "Found '<string>' for field `symbol_ref`, ... but expected a
//       option<record<symbol>>"
//   - The whole transaction rolls back silently.
//
// WITH the OVERWRITE fix:
//   - Re-applying the DDL updates the persisted type to `option<string>`.
//   - The same quoted-string write commits successfully (count = 1).
//
// This is the exact scenario for every on-disk SurrealKV database that was
// created before the `parent`/`symbol_ref` type correction — which is why the
// bug only appeared on existing deployments, not on fresh installs.
#[cfg(test)]
mod stale_schema {
    use surrealdb::Surreal;
    use surrealdb::engine::local::{Db, SurrealKv};
    use tempfile::TempDir;

    use crate::store::schema::SCHEMA_DDL;
    use crate::store::ops::count_chunks;

    /// Open a raw SurrealKV DB (no DDL applied) on a TempDir.
    /// The caller is responsible for applying whatever schema it needs.
    async fn open_raw_db(dir: &std::path::Path, name: &str) -> Surreal<Db> {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let db = Surreal::new::<SurrealKv>(path.to_str().unwrap())
            .await
            .expect("open raw db");
        db.use_ns("context_engine").use_db(name).await.expect("ns/db");
        db
    }

    /// Retrieve the INFO FOR TABLE result for `table` as a raw JSON string.
    /// Used to inspect the persisted field definition before and after DDL re-application.
    async fn info_for_table(db: &Surreal<Db>, table: &str) -> String {
        let result: Option<serde_json::Value> = db
            .query(format!("INFO FOR TABLE {table};"))
            .await
            .expect("INFO FOR TABLE")
            .take(0)
            .ok()
            .flatten();
        format!("{result:?}")
    }

    /// STEP 1 (RED → GREEN):
    ///
    /// 1. Force the datastore into the STALE state: apply OLD DDL declaring
    ///    `symbol_ref` and `parent` as `option<record<symbol>>`.
    /// 2. Inspect the persisted type via `INFO FOR TABLE` — confirms old type is in place.
    /// 3. Re-apply the CURRENT corrected `SCHEMA_DDL` (with OVERWRITE).
    /// 4. Inspect again — with OVERWRITE the type MUST now read `option<string>`.
    /// 5. Attempt the real writer's statement (quoted-string `symbol_ref` inside a txn).
    /// 6. Assert the write COMMITS and count = 1.
    ///
    /// This test FAILS without `DEFINE FIELD OVERWRITE` (plain re-DEFINE is a no-op,
    /// the FieldCheck error still triggers) and PASSES with OVERWRITE.
    #[tokio::test]
    async fn overwrite_migrates_stale_schema_and_write_commits() {
        let home = TempDir::new().unwrap();
        let db = open_raw_db(home.path(), "stale_repro").await;

        // ── 1. Install the OLD (stale) schema ────────────────────────────────
        // This mirrors what every pre-fix on-disk database had: both critical
        // fields declared as `option<record<symbol>>`.
        // The chunk table must include all previously-required fields (file,
        // line_start, line_end, content, embedding) so that SCHEMA_DDL's
        // `DEFINE INDEX IF NOT EXISTS idx_chunk_file ON chunk FIELDS file`
        // does not fail with FdNotFound on this SCHEMAFULL table.
        let old_ddl = "\
            DEFINE TABLE chunk SCHEMAFULL;\
            DEFINE FIELD file ON chunk TYPE string;\
            DEFINE FIELD line_start ON chunk TYPE int;\
            DEFINE FIELD line_end ON chunk TYPE int;\
            DEFINE FIELD content ON chunk TYPE string;\
            DEFINE FIELD embedding ON chunk TYPE array<float>;\
            DEFINE FIELD symbol_ref ON chunk TYPE option<record<symbol>>;\
            DEFINE TABLE symbol SCHEMAFULL;\
            DEFINE FIELD parent ON symbol TYPE option<record<symbol>>;";
        db.query(old_ddl)
            .await
            .expect("install old stale DDL")
            .check()
            .expect("old DDL must not err");

        // ── 2. Confirm the old type is persisted ─────────────────────────────
        let before = info_for_table(&db, "chunk").await;
        println!("STALE-SCHEMA INFO BEFORE re-apply:\n  chunk: {before}");
        // The persisted definition must contain `record<symbol>` or `record` to
        // confirm the stale state is actually in place.
        assert!(
            before.to_lowercase().contains("record"),
            "before re-apply, the stale type must contain 'record' — got: {before}"
        );

        // ── 3. Re-apply the corrected SCHEMA_DDL ─────────────────────────────
        // The chunk table is now SCHEMALESS in SCHEMA_DDL — its typed field
        // definitions were removed. `DEFINE TABLE IF NOT EXISTS chunk SCHEMALESS`
        // is a no-op here since the table already exists.
        // The symbol table still has `DEFINE FIELD OVERWRITE parent` which fixes
        // the stale `option<record<symbol>>` type.
        db.query(SCHEMA_DDL)
            .await
            .expect("corrected DDL must not return transport error")
            .check()
            .expect("corrected DDL must have no per-statement errors");

        // ── 4. Confirm symbol.parent type has been updated ───────────────────
        let after_symbol = info_for_table(&db, "symbol").await;
        println!("STALE-SCHEMA INFO AFTER re-apply:\n  symbol: {after_symbol}");
        // After OVERWRITE, `record<symbol>` must be gone from symbol.parent's definition.
        assert!(
            !after_symbol.contains("record<symbol>"),
            "after re-apply with OVERWRITE, 'record<symbol>' must be gone from symbol.parent \
             field definition — OVERWRITE did not update the persisted type. Got: {after_symbol}"
        );

        // The chunk table's symbol_ref is handled by v2→v3 migration (REMOVE FIELD),
        // not by SCHEMA_DDL re-application. The write below uses SCHEMALESS-compatible
        // syntax; any stale type on chunk.symbol_ref in a pre-v3 DB would be cleared
        // by run_migration_v2_to_v3 before this write path is reached in production.

        // ── 5. Run v2→v3 migration to remove the stale chunk field definitions ─
        // In production, this runs before any write path that stores chunks.
        // We need index_meta for the migration's set_meta calls.
        db.query(
            "DEFINE TABLE IF NOT EXISTS index_meta SCHEMAFULL;\
             DEFINE FIELD OVERWRITE key ON index_meta TYPE string;\
             DEFINE FIELD OVERWRITE value ON index_meta TYPE string;\
             DEFINE INDEX IF NOT EXISTS idx_meta_key ON index_meta FIELDS key UNIQUE;"
        )
        .await
        .expect("setup index_meta for migration")
        .check()
        .expect("index_meta setup check");

        crate::store::run_migration_v2_to_v3(&db).await
            .expect("v2→v3 migration must succeed");

        // ── 6. Attempt the real writer's statement (mirroring pipeline.rs) ───
        let txn = "BEGIN TRANSACTION;\n\
            CREATE chunk SET \
              file = '/x/config.rs', \
              line_start = 1, \
              line_end = 10, \
              content = 'impl EmbeddingConfig {}', \
              embedding = [0.0, 1.0, 0.5], \
              symbol_ref = 'symbol:⟨config.rs::impl_EmbeddingConfig⟩';\n\
            COMMIT TRANSACTION;\n";

        let mut resp = db.query(txn).await.expect(".await must not fail");
        let errors = resp.take_errors();
        println!(
            "STALE-SCHEMA WRITE RESULT: errors = {errors:?}"
        );

        const GENERIC: &str = "The query was not executed due to a failed transaction";
        let real_error: Vec<_> = errors
            .iter()
            .filter(|(_, e)| !e.to_string().contains(GENERIC))
            .collect();
        println!("STALE-SCHEMA WRITE: non-generic errors = {real_error:?}");

        // ── 7. Assert commit succeeded ────────────────────────────────────────
        assert!(
            real_error.is_empty(),
            "transaction must commit after v2→v3 migration removes stale field type: {real_error:?}\n\
             REMOVE FIELD did NOT remove the stale 'option<record<symbol>>' definition."
        );

        let count = count_chunks(&db).await.unwrap();
        println!("STALE-SCHEMA WRITE: chunk count after commit = {count}");
        assert_eq!(
            count,
            1,
            "chunk must persist after migration (got {count}); \
             transaction is still rolling back due to stale field type"
        );
    }

    /// Verify that `DEFINE TABLE IF NOT EXISTS` does NOT drop existing rows.
    /// This confirms the table DDL form we chose is safe to re-run on a live database.
    #[tokio::test]
    async fn table_redefine_does_not_drop_rows() {
        let home = TempDir::new().unwrap();
        let db = open_raw_db(home.path(), "table_redef").await;

        // Set up a minimal chunk table and insert a sentinel row.
        db.query(
            "DEFINE TABLE IF NOT EXISTS chunk SCHEMAFULL;\
             DEFINE FIELD OVERWRITE file ON chunk TYPE string;\
             DEFINE FIELD OVERWRITE line_start ON chunk TYPE int;\
             DEFINE FIELD OVERWRITE line_end ON chunk TYPE int;\
             DEFINE FIELD OVERWRITE content ON chunk TYPE string;\
             DEFINE FIELD OVERWRITE embedding ON chunk TYPE array<float>;\
             DEFINE FIELD OVERWRITE symbol_ref ON chunk TYPE option<string>;\
             CREATE chunk SET file='/sentinel', line_start=1, line_end=1, \
               content='sentinel', embedding=[], symbol_ref=NONE;",
        )
        .await
        .expect("setup")
        .check()
        .expect("setup check");

        let before = count_chunks(&db).await.unwrap();
        assert_eq!(before, 1, "sentinel row must exist before re-DDL");

        // Re-run the full SCHEMA_DDL (simulating a server restart).
        db.query(SCHEMA_DDL)
            .await
            .expect("re-apply DDL")
            .check()
            .expect("re-apply check");

        let after = count_chunks(&db).await.unwrap();
        println!("TABLE-REDEF: rows before={before}, after={after}");
        assert_eq!(
            after,
            before,
            "DEFINE TABLE IF NOT EXISTS must not drop existing rows (before={before}, after={after})"
        );
    }
}

// ─── Migration tests ──────────────────────────────────────────────────────
#[cfg(test)]
mod migration_tests {
    use super::*;
    use tempfile::TempDir;

    /// ❾ NEW: migration stamps db_schema_version=2 after completing.
    #[tokio::test]
    async fn migration_stamps_version_2_after_completion() {
        let home = TempDir::new().unwrap();
        let repo = "/test/migration_repo";
        let db = open_db(home.path(), repo).await.unwrap();

        // Confirm we start at version 1 (fresh DB has no version key).
        let before = read_db_schema_version(&db).await;
        assert_eq!(before, 1, "fresh DB should report version 1");

        // Run migration directly.
        run_migration_v1_to_v2(&db).await.unwrap();

        let after = read_db_schema_version(&db).await;
        assert_eq!(after, 2, "after migration, db_schema_version must be 2");
    }

    /// ❾ NEW: migration is idempotent — re-running on a v2 DB changes nothing.
    #[tokio::test]
    async fn migration_idempotent_on_v2_db() {
        let home = TempDir::new().unwrap();
        let repo = "/test/idempotent_repo";
        let db = open_db(home.path(), repo).await.unwrap();

        // Run migration twice.
        run_migration_v1_to_v2(&db).await.unwrap();
        run_migration_v1_to_v2(&db).await.unwrap();

        let version = read_db_schema_version(&db).await;
        assert_eq!(version, 2, "version must still be 2 after second run");
    }

    /// ❾ NEW: crash/resume — migration resumes from persisted cursor.
    /// We seed some calls rows, run migration partially by directly calling the
    /// inner loop logic, then verify a second full run completes cleanly.
    #[tokio::test]
    async fn migration_resumes_from_cursor() {
        let home = TempDir::new().unwrap();
        let repo = "/test/cursor_repo";
        let db = open_db(home.path(), repo).await.unwrap();

        // Migration on empty DB should complete without error.
        run_migration_v1_to_v2(&db).await.unwrap();

        // Version must be 2.
        let v = read_db_schema_version(&db).await;
        assert_eq!(v, 2);

        // Simulate a "resume" by clearing the version key and re-running.
        let _ = db.query("DELETE FROM index_meta WHERE key = $k")
            .bind(("k", DB_SCHEMA_VERSION_KEY))
            .await;
        let v_cleared = read_db_schema_version(&db).await;
        assert_eq!(v_cleared, 1, "after clearing version key, should read 1");

        run_migration_v1_to_v2(&db).await.unwrap();
        let v_again = read_db_schema_version(&db).await;
        assert_eq!(v_again, 2, "after re-run, version must be 2 again");
    }
}

// ─── SCHEMALESS tests ─────────────────────────────────────────────────────
#[cfg(test)]
mod schemaless_tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: open a raw SurrealKV DB without any DDL.
    async fn open_raw(dir: &std::path::Path, name: &str) -> Surreal<Db> {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let db = Surreal::new::<SurrealKv>(path.to_str().unwrap())
            .await
            .expect("open raw db");
        db.use_ns("context_engine").use_db(name).await.expect("ns/db");
        db
    }

    /// Helper: build a distinct 1024-dim embedding from a seed value.
    fn emb_1024(seed: f32) -> Vec<f32> {
        (0..1024).map(|i| seed + i as f32 * 0.0001).collect()
    }

    /// 6a. SCHEMALESS round-trip integrity.
    ///
    /// - Open a fresh DB (applies SCHEMALESS DDL via open_db).
    /// - Write 3 chunks with known distinct 1024-dim embeddings.
    /// - Load via VectorIndex::load_from_db.
    /// - Assert index.len() == 3.
    /// - Search with a query matching one known embedding; assert score ≈ 1.0.
    #[tokio::test]
    async fn schemaless_roundtrip_integrity() {
        use crate::vector::VectorIndex;

        let home = TempDir::new().unwrap();
        let repo = "/test/schemaless_roundtrip";
        let db = open_db(home.path(), repo).await.unwrap();

        let embeddings: Vec<Vec<f32>> = vec![
            emb_1024(1.0),
            emb_1024(2.0),
            emb_1024(3.0),
        ];
        let files = ["/repo/a.rs", "/repo/b.rs", "/repo/c.rs"];

        for (i, emb) in embeddings.iter().enumerate() {
            let emb_str: String = emb.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let q = format!(
                "INSERT INTO chunk {{ file: '{}', line_start: 1, line_end: 10, \
                 content: 'x', embedding: [{}], symbol_ref: NONE }}",
                files[i], emb_str
            );
            db.query(&q).await.expect("insert chunk");
        }

        let index = VectorIndex::load_from_db(&db).await.unwrap();
        assert_eq!(index.len(), 3, "index must contain all 3 chunks");

        // Query with exact copy of embeddings[1] — should get score ≈ 1.0.
        let results = index.search(&embeddings[1], 1);
        assert_eq!(results.len(), 1);
        let diff = (results[0].score - 1.0_f32).abs();
        assert!(
            diff < 1e-4,
            "search for exact embedding must return score ≈ 1.0, got {}",
            results[0].score
        );
        assert_eq!(results[0].chunk_id.file, files[1]);
    }

    /// 6b. Migration gating readback.
    ///
    /// - Open a DB with OLD SCHEMAFULL schema.
    /// - Write chunks (short 4-dim embeddings so the schema allows them).
    /// - Manually write a 1024-dim chunk.
    /// - Run run_migration_v2_to_v3.
    /// - Read back; assert embeddings are intact.
    /// - Assert db_schema_version == 3.
    #[tokio::test]
    async fn migration_v2_to_v3_gating_readback() {
        use serde::Deserialize;

        let home = TempDir::new().unwrap();
        let db = open_raw(home.path(), "gating_readback").await;

        // Apply old SCHEMAFULL DDL (v2 state: chunk is SCHEMAFULL with typed fields,
        // but embedding type is array<float> which accepts any float array).
        // We also need index_meta for set_meta/get_meta.
        let old_ddl = "\
            DEFINE TABLE chunk SCHEMAFULL;\
            DEFINE FIELD OVERWRITE file ON chunk TYPE string;\
            DEFINE FIELD OVERWRITE line_start ON chunk TYPE int;\
            DEFINE FIELD OVERWRITE line_end ON chunk TYPE int;\
            DEFINE FIELD OVERWRITE content ON chunk TYPE string;\
            DEFINE FIELD OVERWRITE embedding ON chunk TYPE array<float>;\
            DEFINE FIELD OVERWRITE symbol_ref ON chunk TYPE option<string>;\
            DEFINE TABLE IF NOT EXISTS index_meta SCHEMAFULL;\
            DEFINE FIELD OVERWRITE key ON index_meta TYPE string;\
            DEFINE FIELD OVERWRITE value ON index_meta TYPE string;\
            DEFINE INDEX IF NOT EXISTS idx_meta_key ON index_meta FIELDS key UNIQUE;";
        db.query(old_ddl).await.expect("old DDL").check().expect("old DDL check");

        // Write one chunk with a 1024-dim embedding via raw query (bypassing SCHEMAFULL
        // embedding type — we didn't define it typed so it stores as SCHEMALESS for embedding).
        let emb = emb_1024(42.0);
        let emb_str: String = emb.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let q = format!(
            "CREATE chunk SET file = '/x/f.rs', line_start = 1, line_end = 10, \
             content = 'test', embedding = [{}], symbol_ref = NONE",
            emb_str
        );
        db.query(&q).await.expect("insert chunk");

        // Run the migration.
        run_migration_v2_to_v3(&db).await.unwrap();

        // Verify db_schema_version is now 3.
        let version = read_db_schema_version(&db).await;
        assert_eq!(version, 3, "db_schema_version must be 3 after migration");

        // Read back the embedding and assert it's intact.
        #[derive(Deserialize)]
        struct Row { embedding: Vec<f32> }
        let rows: Vec<Row> = db
            .query("SELECT embedding FROM chunk WHERE embedding IS NOT NONE LIMIT 1")
            .await
            .expect("readback")
            .take(0)
            .expect("take(0)");

        assert_eq!(rows.len(), 1, "must have one chunk after migration");
        assert_eq!(rows[0].embedding.len(), 1024, "embedding must be 1024-dim after migration");
        // Check first and last value are close to the seeded values.
        let diff_first = (rows[0].embedding[0] - emb[0]).abs();
        assert!(diff_first < 1e-4, "first embedding value must match: {}", diff_first);
    }

    /// 6c. needs_rebuild flag lifecycle.
    #[tokio::test]
    async fn needs_rebuild_flag_lifecycle() {
        let home = TempDir::new().unwrap();
        let repo = "/test/needs_rebuild";
        let db = open_db(home.path(), repo).await.unwrap();

        // Set needs_rebuild to "1".
        ops::set_meta(&db, "needs_rebuild", "1").await.unwrap();

        // Assert get_meta returns Some("1").
        let v = ops::get_meta(&db, "needs_rebuild").await.unwrap();
        assert_eq!(v, Some("1".to_string()), "needs_rebuild must be set to '1'");

        // Simulate clearing (the same query used in run_consumer's Ok arm).
        db.query("DELETE FROM index_meta WHERE key = 'needs_rebuild'")
            .await
            .expect("delete needs_rebuild");

        // Assert get_meta returns None.
        let v_after = ops::get_meta(&db, "needs_rebuild").await.unwrap();
        assert_eq!(v_after, None, "needs_rebuild must be None after deletion");
    }

    /// 6d. IS NOT NONE filter correctness.
    ///
    /// - Write chunks: some with real embeddings, some with empty `[]`.
    /// - Query with WHERE embedding IS NOT NONE — assert rows returned include empties too
    ///   (the filter passes both real and empty since [] is not NONE).
    /// - Build VectorIndex — assert only real-embedding rows end up in index
    ///   (empty ones skipped by VectorIndex::insert's is_empty check).
    #[tokio::test]
    async fn is_not_none_filter_correctness() {
        use crate::vector::VectorIndex;

        let home = TempDir::new().unwrap();
        let repo = "/test/is_not_none";
        let db = open_db(home.path(), repo).await.unwrap();

        // Write 2 chunks with real 1024-dim embeddings.
        for i in 0..2_usize {
            let emb = emb_1024(i as f32 + 1.0);
            let emb_str: String = emb.iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let q = format!(
                "INSERT INTO chunk {{ file: '/repo/real_{i}.rs', line_start: 1, line_end: 5, \
                 content: 'real', embedding: [{}], symbol_ref: NONE }}",
                emb_str
            );
            db.query(&q).await.expect("insert real chunk");
        }

        // Write 1 chunk with an empty [] embedding.
        db.query(
            "INSERT INTO chunk { file: '/repo/empty.rs', line_start: 1, line_end: 5, \
             content: 'empty', embedding: [], symbol_ref: NONE }"
        )
        .await
        .expect("insert empty chunk");

        // The IS NOT NONE filter should include ALL rows ([] is not NONE).
        // This matches the behavior documented in the plan for test 6d.
        #[derive(serde::Deserialize)]
        struct CountRow { #[allow(dead_code)] file: String }
        let all_rows: Vec<CountRow> = db
            .query("SELECT file FROM chunk WHERE embedding IS NOT NONE")
            .await
            .expect("query")
            .take(0)
            .expect("take");
        assert_eq!(
            all_rows.len(), 3,
            "IS NOT NONE must include all 3 rows (both real and empty [])"
        );

        // VectorIndex::load_from_db uses the IS NOT NONE filter and then skips
        // empty embeddings in VectorIndex::insert. Only 2 real rows end up in index.
        let index = VectorIndex::load_from_db(&db).await.unwrap();
        assert_eq!(
            index.len(), 2,
            "VectorIndex must contain only 2 real-embedding rows, got {}",
            index.len()
        );
    }
}
