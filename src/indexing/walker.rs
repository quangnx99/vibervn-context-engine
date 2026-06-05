use std::path::Path;

use ignore::WalkBuilder;
use tracing::debug;

/// Extensions considered indexable code/config.
pub const CODE_EXTENSIONS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "rs", "go", "java", "cs", "cpp", "c", "h", "hpp", "cc", "cxx", "hxx", "hh",
    "rb", "php", "swift", "kt", "scala", "ex", "exs", "clj", "hs", "ml", "lua", "r",
    "sh", "bash", "zsh", "fish", "ps1", "yaml", "yml", "toml", "json", "xml", "html",
    "css", "scss", "sql", "proto", "graphql", "md", "txt", "dockerfile", "tf", "hcl",
];

/// Directories to always skip.
pub const SKIP_DIRS: &[&str] = &[
    "node_modules", ".git", "target", "build", "dist", "__pycache__",
    ".venv", "vendor", ".cache", ".idea", ".vscode",
];

/// Walk a repository directory and return all indexable file paths.
/// Respects .gitignore and .ignore files via the `ignore` crate.
pub fn walk_repo(repo_path: &str) -> Vec<String> {
    let root = Path::new(repo_path);
    if !root.exists() {
        return vec![];
    }

    let mut files = Vec::new();

    let walker = WalkBuilder::new(root)
        .hidden(false) // include dot-files that aren't gitignored
        .git_ignore(true)
        .git_global(true)
        .ignore(true)
        .filter_entry(|entry| {
            // Skip known non-code directories.
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_str().unwrap_or("");
                return !SKIP_DIRS.contains(&name);
            }
            true
        })
        .build();

    for result in walker {
        match result {
            Ok(entry) => {
                let ft = entry.file_type().unwrap_or_else(|| {
                    // DirEntry without file type — skip.
                    entry.file_type().unwrap()
                });
                if !ft.is_file() {
                    continue;
                }
                let path = entry.path();
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if CODE_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
                        debug!(path = ?path, "discovered file");
                        if let Some(s) = path.to_str() {
                            files.push(s.to_string());
                        }
                    }
                } else {
                    // No extension — check filename (e.g. "Dockerfile")
                    let fname = path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if (fname == "dockerfile"
                        || fname == "makefile"
                        || fname == "justfile")
                        && let Some(s) = path.to_str()
                    {
                        files.push(s.to_string());
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "walk error (skipping)");
            }
        }
    }

    files
}
