use std::sync::Arc;

use clap::Parser;
use tokio::sync::RwLock;
use tracing::info;
use tracing_subscriber::EnvFilter;

use context_engine_rs::{config::ensure_dir_and_load, indexing::IndexEngine, server, store};

#[derive(Parser, Debug)]
#[command(name = "context-engine", about = "Context Engine settings server")]
struct Cli {
    /// Port to listen on [env: CONTEXT_ENGINE_PORT]
    #[arg(long, env = "CONTEXT_ENGINE_PORT")]
    port: Option<u16>,

    /// Bind address [env: CONTEXT_ENGINE_BIND]
    #[arg(long, env = "CONTEXT_ENGINE_BIND")]
    bind: Option<String>,
}

#[tokio::main]
async fn main() {
    // Initialise tracing subscriber — reads RUST_LOG env var for filtering.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("context_engine_rs=info,warn")),
        )
        .init();

    let cli = Cli::parse();

    // Resolve port: CLI flag → env (handled by clap) → default 6699.
    let port = cli.port.unwrap_or(6699);

    // Resolve bind address: CLI flag → env (handled by clap) → default 127.0.0.1.
    let bind = cli.bind.as_deref().unwrap_or("127.0.0.1").to_owned();

    // Home-dir probe: exit early if we can't determine the home directory.
    let home_dir = match dirs::home_dir() {
        Some(h) => h,
        None => {
            eprintln!(
                "error: could not determine user home directory; \
                 set HOME (Unix) or USERPROFILE (Windows)"
            );
            std::process::exit(2);
        }
    };

    // Load settings (needed to know which repos to watch).
    let settings = match ensure_dir_and_load(&home_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not load settings: {e}");
            std::process::exit(2);
        }
    };

    // Wrap the loaded settings in a shared live handle so IndexEngine and the
    // HTTP server share a single source of truth that mutates on every PUT /api/config.
    let settings_handle = Arc::new(RwLock::new(settings));

    // Shared per-repo DB map — starts empty; `get_or_open` populates lazily.
    // The eager open loop has been removed: SurrealDB handles are expensive to
    // open and the consumer + query paths already cache them via `get_or_open`.
    let repo_dbs: store::RepoDbMap = Arc::new(RwLock::new(std::collections::HashMap::new()));

    // Take one owned boot snapshot — the read guard drops at the end of this
    // statement, so it is NOT held across the IndexEngine::start(...).await below.
    let boot_settings = settings_handle.read().await.clone();
    let repo_count = boot_settings.repos.len();

    // Start IndexEngine — spawns watchers for all configured repos.
    // It shares `repo_dbs` so indexer writes land in the handles the server reads.
    // It receives the shared settings handle so the consumer picks up post-boot changes.
    let index_engine = IndexEngine::start(
        home_dir.clone(),
        &boot_settings,
        repo_dbs.clone(),
        settings_handle.clone(),
    )
    .await;
    info!("IndexEngine started ({} repos)", repo_count);

    let addr: std::net::SocketAddr = format!("{bind}:{port}")
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("error: invalid bind address '{bind}:{port}': {e}");
            std::process::exit(2);
        });

    let app = server::build_router(home_dir, index_engine, repo_dbs, settings_handle, &bind);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("error: could not bind to {addr}: {e}");
            std::process::exit(2);
        });

    info!("Context Engine listening on http://{addr}");
    axum::serve(listener, app).await.unwrap_or_else(|e| {
        eprintln!("server error: {e}");
        std::process::exit(1);
    });
}
