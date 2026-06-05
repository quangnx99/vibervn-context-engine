use std::path::{Component, Path};

use ignore::WalkBuilder;
use tracing::debug;

/// Extensions considered indexable code/config.
pub const CODE_EXTENSIONS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "rs", "go", "java", "cs", "cpp", "c", "h", "hpp", "cc", "cxx", "hxx", "hh",
    "rb", "php", "swift", "kt", "scala", "ex", "exs", "clj", "hs", "ml", "lua", "r",
    "sh", "bash", "zsh", "fish", "ps1", "yaml", "yml", "toml", "json", "xml", "html",
    "css", "scss", "sql", "proto", "graphql", "md", "txt", "dockerfile", "tf", "hcl",
];

/// Non-dot directories to always skip (dot-prefixed directories are pruned by the
/// walk filter automatically, so they do not need to appear here).
pub const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", "build", "dist", "__pycache__", "vendor",
];

/// Returns true if `path` is inside a dot-prefixed directory relative to `repo_root`.
/// The check looks ONLY at directory components between `repo_root` and the file's
/// parent — the file's own basename does not count, so a root-level file like
/// `.eslintrc.json` returns false. The repo root itself is never considered hidden.
/// Returns false if `path` is not inside `repo_root` (caller's responsibility).
pub fn is_under_hidden_dir(repo_root: &Path, path: &Path) -> bool {
    let relative = match path.strip_prefix(repo_root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    // Iterate all components except the last one (the filename).
    let components: Vec<_> = relative.components().collect();
    let dir_components = if components.is_empty() {
        &[][..]
    } else {
        &components[..components.len() - 1]
    };
    for component in dir_components {
        if let Component::Normal(name) = component
            && name.to_str().map(|s| s.starts_with('.')).unwrap_or(false)
        {
            return true;
        }
    }
    false
}

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
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_str().unwrap_or("");
                // Skip non-dot directories from the explicit list.
                if SKIP_DIRS.contains(&name) {
                    return false;
                }
                // Prune any dot-prefixed directory that is NOT the repo root itself.
                // depth() == 0 means this entry IS the root — never prune it, even if
                // the root folder's own name starts with '.'.
                if entry.depth() > 0 && name.starts_with('.') {
                    return false;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Normalize a path string to use forward slashes (for cross-platform comparison).
    fn fwd(s: &str) -> String {
        s.replace('\\', "/")
    }

    /// Create a file at `dir/rel_path`, creating parent directories as needed.
    fn touch(dir: &std::path::Path, rel_path: &str) {
        let full = dir.join(rel_path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::File::create(&full).unwrap();
    }

    #[test]
    fn walk_repo_skips_dot_directories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(root, "src/main.rs");
        touch(root, ".eslintrc.json");
        touch(root, ".claude/agents/x.md");
        touch(root, ".agent/y.py");
        touch(root, ".github/workflows/ci.yml");

        let repo_str = root.to_str().unwrap();
        let result: Vec<String> = walk_repo(repo_str).into_iter().map(|p| fwd(&p)).collect();

        // src/main.rs must be indexed.
        let has_main = result.iter().any(|p| p.ends_with("src/main.rs"));
        assert!(has_main, "src/main.rs must be indexed; got: {:?}", result);

        // Root-level dot-file must be indexed.
        let has_eslintrc = result.iter().any(|p| p.ends_with(".eslintrc.json"));
        assert!(has_eslintrc, ".eslintrc.json must be indexed (root-level dot-file); got: {:?}", result);

        // Nothing under .claude, .agent, or .github must appear.
        for p in &result {
            assert!(
                !p.contains("/.claude/") && !p.contains("\\.claude\\"),
                ".claude/ content must not be indexed; got path: {p}"
            );
            assert!(
                !p.contains("/.agent/") && !p.contains("\\.agent\\"),
                ".agent/ content must not be indexed; got path: {p}"
            );
            assert!(
                !p.contains("/.github/") && !p.contains("\\.github\\"),
                ".github/ content must not be indexed; got path: {p}"
            );
        }
    }

    #[test]
    fn walk_repo_still_skips_node_modules() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        touch(root, "node_modules/foo/index.js");
        touch(root, "src/main.js");

        let repo_str = root.to_str().unwrap();
        let result: Vec<String> = walk_repo(repo_str).into_iter().map(|p| fwd(&p)).collect();

        let has_main = result.iter().any(|p| p.ends_with("src/main.js"));
        assert!(has_main, "src/main.js must be indexed; got: {:?}", result);

        let has_node_modules = result.iter().any(|p| p.contains("node_modules"));
        assert!(!has_node_modules, "node_modules content must not be indexed; got: {:?}", result);
    }

    #[test]
    fn walk_repo_works_when_root_name_is_dotted() {
        let tmp = TempDir::new().unwrap();
        // Create a subdirectory with a dot-prefixed name to use as the repo root.
        let dotted_root = tmp.path().join(".dotted-root");
        fs::create_dir_all(&dotted_root).unwrap();
        touch(&dotted_root, "src/main.rs");

        let repo_str = dotted_root.to_str().unwrap();
        let result: Vec<String> = walk_repo(repo_str).into_iter().map(|p| fwd(&p)).collect();

        let has_main = result.iter().any(|p| p.ends_with("src/main.rs"));
        assert!(
            has_main,
            "src/main.rs must be indexed even when repo root name starts with '.'; got: {:?}",
            result
        );
    }

    #[test]
    fn is_under_hidden_dir_helper() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Create actual paths so strip_prefix works correctly on all platforms.
        let claude_dir = root.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let x_md = claude_dir.join("x.md");
        fs::File::create(&x_md).unwrap();

        let eslintrc = root.join(".eslintrc.json");
        fs::File::create(&eslintrc).unwrap();

        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let foo_rs = src_dir.join("foo.rs");
        fs::File::create(&foo_rs).unwrap();

        let nested_dot_dir = root.join("a").join(".b");
        fs::create_dir_all(&nested_dot_dir).unwrap();
        let c_rs = nested_dot_dir.join("c.rs");
        fs::File::create(&c_rs).unwrap();

        // .claude/x.md — inside a dot-dir → true
        assert!(
            is_under_hidden_dir(root, &x_md),
            ".claude/x.md must be under a hidden dir"
        );

        // .eslintrc.json at repo root — root-level dot-FILE → false
        assert!(
            !is_under_hidden_dir(root, &eslintrc),
            ".eslintrc.json is a root-level dot-file, not under a hidden dir"
        );

        // src/foo.rs — normal file → false
        assert!(
            !is_under_hidden_dir(root, &foo_rs),
            "src/foo.rs must not be under a hidden dir"
        );

        // a/.b/c.rs — nested dot-dir → true
        assert!(
            is_under_hidden_dir(root, &c_rs),
            "a/.b/c.rs must be under a hidden dir"
        );
    }
}
