use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::Client;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use context_engine_rs::config::{Settings, config_path};
use context_engine_rs::indexing::IndexEngine;
use context_engine_rs::server::build_router;
use context_engine_rs::store::RepoDbMap;

/// Boot the axum server bound to port 0 (OS assigns a free port).
/// Returns the bound address and a join handle for the server task.
async fn start_server(home: &TempDir) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("no local addr");
    let settings = Settings::default();
    let settings_handle = Arc::new(RwLock::new(settings.clone()));
    let repo_dbs: RepoDbMap = Arc::new(RwLock::new(HashMap::new()));
    let index_engine = IndexEngine::start(home.path().to_path_buf(), &settings, repo_dbs.clone(), settings_handle.clone()).await;
    let app = build_router(
        home.path().to_path_buf(),
        index_engine,
        repo_dbs,
        settings_handle,
        "127.0.0.1",
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server error");
    });
    addr
}

// ─── Test 1: GET creates default settings ────────────────────────────────
#[tokio::test]
async fn test_get_creates_default() {
    let home = TempDir::new().expect("tempdir");
    let addr = start_server(&home).await;

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");

    let res = client
        .get(format!("http://{addr}/api/config"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(res.status().as_u16(), 200);

    let body: serde_json::Value = res.json().await.expect("parse json");

    // version should be 1
    assert_eq!(body["version"], 1);

    // repos should be an empty array
    assert!(body["repos"].as_array().map(|a| a.is_empty()).unwrap_or(false));

    // settings.json should have been created on disk
    let settings_path = config_path(home.path());
    assert!(settings_path.exists(), "settings.json was not created");

    // Deserialize from disk and verify it matches defaults
    let on_disk: Settings =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).expect("read"))
            .expect("deserialize");
    assert_eq!(on_disk, Settings::default());
}

// ─── Test 2: PUT round-trip ────────────────────────────────────────────────
#[tokio::test]
async fn test_put_round_trips() {
    let home = TempDir::new().expect("tempdir");
    let addr = start_server(&home).await;

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");

    // Seed with a GET so the file is created.
    client
        .get(format!("http://{addr}/api/config"))
        .send()
        .await
        .expect("initial GET");

    let payload = serde_json::json!({
        "version": 1,
        "repos": ["/home/user/myproject", "/home/user/other"],
        "embedding": {
            "provider": "voyage",
            "model": "voyage-code-3",
            "api_keys": ["key-abc-1234", "key-xyz-5678"]
        },
        "llm": {
            "provider": "google",
            "rerank_model": "gemini-2.0-flash",
            "api_keys": ["google-api-key-9999"]
        }
    });

    let put_res = client
        .put(format!("http://{addr}/api/config"))
        .json(&payload)
        .send()
        .await
        .expect("PUT request");

    assert_eq!(put_res.status().as_u16(), 200, "PUT should return 200");

    let put_body: Settings = put_res.json().await.expect("parse PUT response");

    // GET to verify round-trip
    let get_res = client
        .get(format!("http://{addr}/api/config"))
        .send()
        .await
        .expect("GET after PUT");

    assert_eq!(get_res.status().as_u16(), 200);

    let get_body: Settings = get_res.json().await.expect("parse GET response");

    assert_eq!(put_body, get_body, "PUT response and subsequent GET should be equal");
    assert_eq!(get_body.repos, vec!["/home/user/myproject", "/home/user/other"]);
    assert_eq!(get_body.embedding.model, "voyage-code-3");
    assert_eq!(get_body.llm.rerank_model, "gemini-2.0-flash");
    assert_eq!(get_body.version, 1);
}

// ─── Test 3 (Unix only): file mode bits should be 0o600 ───────────────────
#[cfg(unix)]
#[tokio::test]
async fn test_unix_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let home = TempDir::new().expect("tempdir");
    let addr = start_server(&home).await;

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");

    // GET creates the default file.
    let res = client
        .get(format!("http://{addr}/api/config"))
        .send()
        .await
        .expect("GET");

    assert_eq!(res.status().as_u16(), 200);

    let settings_path = config_path(home.path());
    let meta = std::fs::metadata(&settings_path).expect("stat settings.json");
    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "settings.json should have mode 0o600, got 0o{mode:o}");
}

// ─── Test 4: PUT a repo → subsequent query sees it (live settings) ────────
//
// Boot server with no repos. PUT a config that includes one real temp-dir repo.
// POST /api/query — expect "Index is empty" (NOT "No repositories configured").
// This proves post_query reads live settings, not the frozen boot-time snapshot.
//
// Preflight order in post_query:
//   1. repos.is_empty()          → "No repositories configured."
//   2. vector_index.is_empty()   → "Index is empty. Trigger indexing first."
//   3. api_keys.is_empty()       → "No embedding API keys configured."
// With a freshly-added repo and empty vector index, check (1) passes, (2) fires.
#[tokio::test]
async fn test_put_repo_then_query_passes_preflight() {
    let home = TempDir::new().expect("tempdir");
    let repo_dir = TempDir::new().expect("repo tempdir");
    let addr = start_server(&home).await;

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");

    // Seed the config file via GET so it exists on disk.
    client
        .get(format!("http://{addr}/api/config"))
        .send()
        .await
        .expect("initial GET");

    // PUT a config that includes one real directory as a repo.
    let repo_path = repo_dir.path().to_string_lossy().to_string();
    let payload = serde_json::json!({
        "version": 1,
        "repos": [repo_path],
        "embedding": {
            "provider": "voyage",
            "model": "voyage-4-lite",
            "api_keys": ["key-test-1234"]
        },
        "llm": {
            "provider": "google",
            "rerank_model": "gemini-3.1-flash-lite",
            "api_keys": []
        }
    });

    let put_res = client
        .put(format!("http://{addr}/api/config"))
        .json(&payload)
        .send()
        .await
        .expect("PUT request");

    assert_eq!(put_res.status().as_u16(), 200, "PUT should return 200");

    // Now POST /api/query. The repos check must pass (repo was added).
    // The vector index is empty (no actual indexing happened), so we expect
    // "Index is empty. Trigger indexing first." — NOT "No repositories configured."
    let query_res = client
        .post(format!("http://{addr}/api/query"))
        .json(&serde_json::json!({ "query": "x", "top_k": 5 }))
        .send()
        .await
        .expect("query request");

    let status = query_res.status().as_u16();
    let body: serde_json::Value = query_res.json().await.expect("parse query response");
    let error_msg = body["error"].as_str().unwrap_or("");

    // Must NOT be "No repositories configured" — that would mean we're reading stale settings.
    assert!(
        !error_msg.contains("No repositories configured"),
        "Expected live settings to be used but got: {error_msg}"
    );

    // Must be the vector-index empty error — proves repos preflight passed.
    assert_eq!(status, 400);
    assert!(
        error_msg.contains("Index is empty"),
        "Expected 'Index is empty' but got: {error_msg}"
    );
}

// ─── Test 5: PUT a repo → register_repo fires → status entry exists ───────
//
// PUT a config with a real temp directory as a repo. After the PUT returns 200,
// GET /api/index-status and assert that an entry for that repo path exists.
// This proves register_repo was called from put_config (not just next restart).
#[tokio::test]
async fn test_put_repo_registers_status() {
    let home = TempDir::new().expect("tempdir");
    let repo_dir = TempDir::new().expect("repo tempdir");
    let addr = start_server(&home).await;

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");

    // Seed config file.
    client
        .get(format!("http://{addr}/api/config"))
        .send()
        .await
        .expect("initial GET");

    let repo_path = repo_dir.path().to_string_lossy().to_string();
    let payload = serde_json::json!({
        "version": 1,
        "repos": [repo_path],
        "embedding": { "provider": "voyage", "model": "voyage-4-lite", "api_keys": [] },
        "llm": { "provider": "google", "rerank_model": "gemini-3.1-flash-lite", "api_keys": [] }
    });

    let put_res = client
        .put(format!("http://{addr}/api/config"))
        .json(&payload)
        .send()
        .await
        .expect("PUT request");

    assert_eq!(put_res.status().as_u16(), 200, "PUT should return 200");

    // register_repo is awaited inside put_config before the response is sent,
    // so the status entry must exist immediately after the 200 response.
    let status_res = client
        .get(format!("http://{addr}/api/index-status"))
        .send()
        .await
        .expect("index-status request");

    assert_eq!(status_res.status().as_u16(), 200);

    let statuses: Vec<serde_json::Value> = status_res.json().await.expect("parse status response");
    let found = statuses.iter().any(|s| {
        s["repo"].as_str().map(|r| r == repo_path).unwrap_or(false)
    });

    assert!(
        found,
        "Expected a status entry for {repo_path} but got: {statuses:?}"
    );
}
