pub mod chunker;
pub mod relations;
pub mod symbols;

use std::collections::HashMap;
use std::path::Path;

use tracing::{warn};
use tree_sitter::{Node, Parser};

use crate::parsing::chunker::{Chunk, chunk_file};
use crate::parsing::relations::{EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};

/// Result of parsing one source file.
#[derive(Debug)]
pub struct ParseResult {
    pub symbols: Vec<Symbol>,
    pub edges: Vec<RawEdge>,
    pub chunks: Vec<Chunk>,
    /// Import map: local name → source file path (best-effort, only for resolved imports).
    pub imports: HashMap<String, String>,
}

// ─── Language detection ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    Other,
}

pub fn detect_language(path: &Path) -> Lang {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Lang::Python,
        Some("js" | "jsx" | "mjs" | "cjs") => Lang::JavaScript,
        Some("ts") => Lang::TypeScript,
        Some("tsx") => Lang::Tsx,
        Some("rs") => Lang::Rust,
        Some("go") => Lang::Go,
        Some("java") => Lang::Java,
        Some("c") => Lang::C,
        Some("cpp" | "cc" | "cxx") => Lang::Cpp,
        Some("h" | "hpp" | "hxx" | "hh") => Lang::Cpp,
        _ => Lang::Other,
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────

/// Parse a source file and return symbols, edges, and chunks.
/// Falls back to coverage-only chunks on parse failure.
pub fn parse_file(file_path: &str, source: &str) -> ParseResult {
    let path = Path::new(file_path);
    let lang = detect_language(path);

    let (symbols, edges, imports) = match lang {
        Lang::Python => {
            let (s, e) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_python::LANGUAGE.into(),
                extract_python,
            );
            (s, e, HashMap::new())
        }
        Lang::JavaScript | Lang::Tsx => {
            let (s, e) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_javascript::LANGUAGE.into(),
                extract_javascript,
            );
            (s, e, HashMap::new())
        }
        Lang::TypeScript => {
            let (s, e) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                extract_typescript,
            );
            (s, e, HashMap::new())
        }
        Lang::Rust => {
            let (s, e) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_rust::LANGUAGE.into(),
                extract_rust,
            );
            (s, e, HashMap::new())
        }
        Lang::Go => {
            let (s, e) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_go::LANGUAGE.into(),
                extract_go,
            );
            (s, e, HashMap::new())
        }
        Lang::Java => {
            let (s, e) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_java::LANGUAGE.into(),
                extract_java,
            );
            (s, e, HashMap::new())
        }
        Lang::C => {
            let (s, e, imp) = parse_with_tree_sitter_c_cpp(
                file_path,
                source,
                tree_sitter_c::LANGUAGE.into(),
            );
            (s, e, imp)
        }
        Lang::Cpp => {
            let (s, e, imp) = parse_with_tree_sitter_c_cpp(
                file_path,
                source,
                tree_sitter_cpp::LANGUAGE.into(),
            );
            (s, e, imp)
        }
        Lang::Other => (vec![], vec![], HashMap::new()),
    };

    let chunks = chunk_file(file_path, source, &symbols);

    ParseResult {
        symbols,
        edges,
        chunks,
        imports,
    }
}

// ─── Generic tree-sitter driver ───────────────────────────────────────────

fn parse_with_tree_sitter<F>(
    file_path: &str,
    source: &str,
    language: tree_sitter::Language,
    extractor: F,
) -> (Vec<Symbol>, Vec<RawEdge>)
where
    F: Fn(&str, &str, &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>),
{
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&language) {
        warn!(file = file_path, error = %e, "failed to set tree-sitter language");
        return (vec![], vec![]);
    }
    match parser.parse(source, None) {
        Some(tree) => extractor(file_path, source, &tree),
        None => {
            warn!(file = file_path, "tree-sitter parse returned None");
            (vec![], vec![])
        }
    }
}

/// Specialised tree-sitter driver for C/C++ that also returns the imports HashMap.
fn parse_with_tree_sitter_c_cpp(
    file_path: &str,
    source: &str,
    language: tree_sitter::Language,
) -> (Vec<Symbol>, Vec<RawEdge>, HashMap<String, String>) {
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&language) {
        warn!(file = file_path, error = %e, "failed to set tree-sitter language for C/C++");
        return (vec![], vec![], HashMap::new());
    }
    match parser.parse(source, None) {
        Some(tree) => extract_c_cpp(file_path, source, &tree),
        None => {
            warn!(file = file_path, "tree-sitter parse returned None for C/C++");
            (vec![], vec![], HashMap::new())
        }
    }
}

// ─── Utility helpers ──────────────────────────────────────────────────────

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn node_line_start(node: &Node) -> u32 {
    node.start_position().row as u32 + 1
}

fn node_line_end(node: &Node) -> u32 {
    node.end_position().row as u32 + 1
}

#[allow(clippy::too_many_arguments)]
fn make_symbol(
    file: &str,
    name: &str,
    scope_path: Vec<String>,
    kind: SymbolKind,
    line_start: u32,
    line_end: u32,
    signature: Option<String>,
    parent_fqn: Option<String>,
) -> Symbol {
    Symbol {
        qualified: QualifiedSymbol {
            file: file.to_string(),
            scope_path,
            name: name.to_string(),
        },
        kind,
        line_start,
        line_end,
        signature,
        parent_fqn,
    }
}

// ─── Python extractor ─────────────────────────────────────────────────────

fn extract_python(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_python_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_python_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_definition" | "async_function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if !scope.is_empty() { SymbolKind::Method } else { SymbolKind::Function };
                let sym = make_symbol(
                    file, &name, scope.to_vec(), kind,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Class,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "call" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

fn scope_to_qualified(file: &str, scope: &[String]) -> Option<QualifiedSymbol> {
    scope.last().map(|name| QualifiedSymbol {
        file: file.to_string(),
        scope_path: scope[..scope.len() - 1].to_vec(),
        name: name.clone(),
    })
}

// ─── JavaScript extractor ─────────────────────────────────────────────────

fn extract_javascript(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_js_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_js_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_declaration" | "function" => {
            let name = node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let kind = if !scope.is_empty() { SymbolKind::Method } else { SymbolKind::Function };
            let sym = make_symbol(
                file, &name, scope.to_vec(), kind,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "class_declaration" | "class" => {
            let name = node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let sym = make_symbol(
                file, &name, scope.to_vec(), SymbolKind::Class,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── TypeScript extractor ─────────────────────────────────────────────────

fn extract_typescript(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    // TypeScript grammar is a superset of JS grammar — reuse JS extractor.
    extract_javascript(file, source, tree)
}

// ─── Rust extractor ───────────────────────────────────────────────────────

fn extract_rust(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_rust_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_rust_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if scope.iter().any(|s| s.starts_with("impl")) {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file, &name, scope.to_vec(), kind,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "struct_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Struct,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                symbols.push(sym);
            }
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Trait,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "impl_item" => {
            let type_name = node.child_by_field_name("type")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "impl".to_string());
            let impl_name = format!("impl_{}", type_name);
            let sym = make_symbol(
                file, &impl_name, scope.to_vec(), SymbolKind::Impl,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(impl_name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Module,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Go extractor ─────────────────────────────────────────────────────────

fn extract_go(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_go_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_go_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            let name = node.child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anon>".to_string());
            let kind = if node.kind() == "method_declaration" {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sym = make_symbol(
                file, &name, scope.to_vec(), kind,
                node_line_start(node), node_line_end(node),
                None, parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
            }
        }
        "type_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(&name_node, source).to_string();
                    let sym = make_symbol(
                        file, &name, scope.to_vec(), SymbolKind::Struct,
                        node_line_start(&child), node_line_end(&child),
                        None, parent_fqn.map(|s| s.to_string()),
                    );
                    symbols.push(sym);
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Java extractor ───────────────────────────────────────────────────────

fn extract_java(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_java_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_java_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    match node.kind() {
        "class_declaration" | "interface_declaration" => {
            let kind = if node.kind() == "interface_declaration" {
                SymbolKind::Interface
            } else {
                SymbolKind::Class
            };
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), kind,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_java_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file, &name, scope.to_vec(), SymbolKind::Method,
                    node_line_start(node), node_line_end(node),
                    None, parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_java_node(file, source, &child, &child_scope, Some(&fqn), symbols, edges);
                }
            }
        }
        "method_invocation" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = node_text(&name_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_java_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_java_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── C / C++ extractor ────────────────────────────────────────────────────

fn extract_c_cpp(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>, HashMap<String, String>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut imports: HashMap<String, String> = HashMap::new();
    let root = tree.root_node();
    extract_c_cpp_node(file, source, &root, &[], None, &mut symbols, &mut edges, &mut imports);
    (symbols, edges, imports)
}

/// Drill through nested declarator nodes to find the leaf identifier/name.
/// Returns (name_text, is_qualified) where is_qualified means we saw a
/// qualified_identifier along the way (Foo::bar).
fn declarator_name<'a>(node: &Node, source: &'a str) -> Option<(&'a str, bool)> {
    match node.kind() {
        "identifier" | "field_identifier" => Some((node_text(node, source), false)),
        "destructor_name" => Some((node_text(node, source), false)),
        "qualified_identifier" => {
            // Rightmost identifier is the leaf name; the rest is scope.
            // qualified_identifier has a `scope` field (left side) and a `name` field (right side).
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                Some((name, true))
            } else {
                None
            }
        }
        "function_declarator" => {
            // function_declarator has its own `declarator` field — recurse.
            node.child_by_field_name("declarator")
                .and_then(|inner| declarator_name(&inner, source))
        }
        "pointer_declarator" | "reference_declarator" | "abstract_reference_declarator" => {
            // pointer/ref: the actual declarator is the last named child.
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find_map(|child| declarator_name(&child, source))
        }
        _ => None,
    }
}

/// For a qualified_identifier, extract the scope prefix (everything before last ::)
/// as a Vec<String> to be prepended to the symbol's scope_path.
fn qualified_scope_prefix(node: &Node, source: &str) -> Vec<String> {
    // The tree-sitter C++ grammar represents `Foo::Bar::baz` as nested
    // qualified_identifiers: scope=qualified_identifier(Foo::Bar), name=baz.
    // We collect the scope chain into a flat Vec.
    let mut parts = Vec::new();
    collect_scope_parts(node, source, &mut parts);
    // The last element is the name itself, not the scope prefix — drop it.
    if !parts.is_empty() {
        parts.pop();
    }
    parts
}

fn collect_scope_parts(node: &Node, source: &str, parts: &mut Vec<String>) {
    match node.kind() {
        "qualified_identifier" => {
            if let Some(scope_node) = node.child_by_field_name("scope") {
                collect_scope_parts(&scope_node, source, parts);
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                parts.push(node_text(&name_node, source).to_string());
            }
        }
        "identifier" | "namespace_identifier" | "type_identifier" => {
            parts.push(node_text(node, source).to_string());
        }
        _ => {}
    }
}

/// Extract the leaf callee name from a call_expression's `function` node.
/// Returns None if the callee cannot be resolved to a simple name.
fn callee_leaf_name<'a>(func_node: &Node, source: &'a str) -> Option<&'a str> {
    match func_node.kind() {
        "identifier" => Some(node_text(func_node, source)),
        "field_expression" => {
            // obj.method() or ptr->method() — the `field` child holds the method name.
            func_node.child_by_field_name("field").map(|n| node_text(&n, source))
        }
        "qualified_identifier" => {
            // ns::Foo::bar() → recursively unwrap to the rightmost leaf identifier.
            // The C++ grammar nests: qualified_identifier(scope=qualified_identifier(ns::Foo), name=bar)
            // or in some grammars: qualified_identifier(scope=ns, name=qualified_identifier(Foo::bar))
            // We always want the final leaf identifier.
            if let Some(name_node) = func_node.child_by_field_name("name") {
                // If name_node is itself a qualified_identifier, recurse.
                callee_leaf_name(&name_node, source)
            } else {
                None
            }
        }
        "template_function" => {
            // template call like foo<T>() — the `name` child holds the base name.
            let name_node = func_node.child_by_field_name("name")?;
            callee_leaf_name(&name_node, source)
        }
        _ => None,
    }
}

/// Return the basename (filename without extension) of an include path.
/// E.g. `"linux/list.h"` → `"list"`, `"Agent.h"` → `"Agent"`.
fn include_basename(path: &str) -> &str {
    // Get the final component after the last `/`.
    let filename = path.rfind('/').map(|i| &path[i + 1..]).unwrap_or(path);
    // Strip the extension (last `.` and everything after).
    filename.rfind('.').map(|i| &filename[..i]).unwrap_or(filename)
}

/// For a qualified call `ns::Foo::method()`, extract the direct qualifier
/// (the part immediately before the leaf name, e.g. `"Foo"` from `ns::Foo::method`).
/// For a field expression `obj.method()` or `ptr->method()`, no qualifier is relevant.
/// Returns None for unqualified or field-expression calls.
fn extract_call_qualifier<'a>(func_node: &Node, source: &'a str) -> Option<&'a str> {
    match func_node.kind() {
        "qualified_identifier" => {
            // Walk down through nested qualified_identifiers to find the deepest one
            // (which is the one whose scope is the direct qualifier of the leaf name).
            //
            // For `ns::Foo::method`:
            //   top: scope=ns, name=qualified_identifier(Foo::method)
            //     inner: scope=Foo, name=method
            // We want the inner's scope ("Foo").
            //
            // For `Agent::run`:
            //   scope=Agent, name=run
            // We want the scope ("Agent").
            //
            // Strategy: recurse into the `name` field if it's a qualified_identifier;
            // otherwise, this IS the innermost qualified_identifier — return its scope text.
            let name_node = func_node.child_by_field_name("name")?;
            if name_node.kind() == "qualified_identifier" {
                // Go deeper.
                extract_call_qualifier(&name_node, source)
            } else {
                // This is the innermost qualified_identifier. Its scope is the direct qualifier.
                let scope_node = func_node.child_by_field_name("scope")?;
                // Get the rightmost component of the scope (in case scope itself is nested).
                match scope_node.kind() {
                    "namespace_identifier" | "type_identifier" | "identifier" => {
                        Some(node_text(&scope_node, source))
                    }
                    "qualified_identifier" => {
                        // Scope is also nested: get the name (rightmost) of the scope.
                        scope_node.child_by_field_name("name").map(|n| node_text(&n, source))
                    }
                    _ => None,
                }
            }
        }
        "template_function" => {
            let name_node = func_node.child_by_field_name("name")?;
            extract_call_qualifier(&name_node, source)
        }
        _ => None,
    }
}

/// Determine the `import_path` for a call expression by checking the imports HashMap.
///
/// Rules (in order):
///  1. If the callee is qualified (e.g. `GoalAgent::doThing()`), extract the direct
///     qualifier (e.g. `GoalAgent`). Check if any include path's basename matches it.
///     If so, set import_path to the full include path.
///  2. If the callee is unqualified (e.g. `foo()`), check if any include path's basename
///     matches `callee_name` (i.e. `callee_name.h` pattern via basename comparison).
///  3. Otherwise, return None.
fn resolve_import_path_for_call(
    func_node: &Node,
    source: &str,
    callee_name: &str,
    imports: &HashMap<String, String>,
) -> Option<String> {
    if imports.is_empty() {
        return None;
    }

    // Try qualified match first.
    if let Some(qualifier) = extract_call_qualifier(func_node, source) {
        // Check if any include basename matches the qualifier.
        for include_path in imports.keys() {
            if include_basename(include_path) == qualifier {
                return Some(include_path.clone());
            }
        }
    }

    // Unqualified call: check if `<callee_name>.h` matches any include basename.
    // We compare the callee_name against include basenames (i.e. basename("Foo.h") == "Foo").
    // Only match if the function is a plain identifier (not qualified or field-based).
    if func_node.kind() == "identifier" {
        for include_path in imports.keys() {
            if include_basename(include_path) == callee_name {
                return Some(include_path.clone());
            }
        }
    }

    None
}

#[allow(clippy::too_many_arguments)]
fn extract_c_cpp_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
    imports: &mut HashMap<String, String>,
) {
    match node.kind() {
        "preproc_include" => {
            // Extract the path child node and strip surrounding `""` or `<>`.
            if let Some(path_node) = node.child_by_field_name("path") {
                let raw = node_text(&path_node, source);
                let stripped = raw.trim_matches(|c| c == '"' || c == '<' || c == '>');
                if !stripped.is_empty() {
                    imports.insert(stripped.to_string(), stripped.to_string());
                }
            }
            // No further children to recurse into for preproc_include.
        }
        "function_definition" => {
            // The outer declarator field is typically a function_declarator.
            if let Some(outer_decl) = node.child_by_field_name("declarator")
                && let Some((name, is_qualified)) = declarator_name(&outer_decl, source)
            {
                let (sym_scope, sym_kind) = if is_qualified {
                    // Out-of-line definition like `Foo::bar(...)` — extract scope prefix.
                    let qscope = qualified_scope_prefix(&outer_decl, source);
                    let mut merged = scope.to_vec();
                    merged.extend(qscope);
                    (merged, SymbolKind::Method)
                } else {
                    let kind = if scope.iter().any(|s| {
                        // Inside a class_specifier or struct_specifier scope.
                        symbols.iter().any(|sym| {
                            sym.qualified.name == *s
                                && matches!(sym.kind, SymbolKind::Class | SymbolKind::Struct)
                        })
                    }) {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    (scope.to_vec(), kind)
                };

                let sym = make_symbol(
                    file,
                    name,
                    sym_scope,
                    sym_kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);

                let mut child_scope = scope.to_vec();
                child_scope.push(name.to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                        imports,
                    );
                }
                return;
            }
            // Fallthrough: declarator not resolved — still recurse for nested nodes.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(file, source, &child, scope, parent_fqn, symbols, edges, imports);
            }
        }
        "class_specifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                        imports,
                    );
                }
            } else {
                // Anonymous class — still recurse without pushing scope.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(file, source, &child, scope, parent_fqn, symbols, edges, imports);
                }
            }
        }
        "struct_specifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Struct,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                        imports,
                    );
                }
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(file, source, &child, scope, parent_fqn, symbols, edges, imports);
                }
            }
        }
        "namespace_definition" => {
            // C++ namespaces — `name` field may be absent (anonymous namespace).
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                SymbolKind::Module,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                    imports,
                );
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function")
                && let Some(callee_name) = callee_leaf_name(&func_node, source)
                && let Some(from_sym) = scope_to_qualified(file, scope)
            {
                // Determine import_path by checking the imports HashMap.
                // For qualified calls (e.g., GoalAgent::doThing()), extract the qualifier
                // (everything before the last ::) and check if any import's basename
                // (without extension) matches it.
                // For unqualified calls, check if <callee_name>.h matches any import basename.
                let import_path = resolve_import_path_for_call(&func_node, source, callee_name, imports);

                edges.push(RawEdge {
                    from: from_sym,
                    to: EdgeTarget::Unresolved {
                        name: callee_name.to_string(),
                        import_path,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line: node_line_start(node),
                });
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(file, source, &child, scope, parent_fqn, symbols, edges, imports);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(file, source, &child, scope, parent_fqn, symbols, edges, imports);
            }
        }
    }
}

// ─── C / C++ unit tests ───────────────────────────────────────────────────

#[cfg(test)]
mod cpp_tests {
    use super::*;
    use crate::parsing::symbols::SymbolKind;
    use crate::parsing::relations::{EdgeTarget, EdgeKind};

    /// Helper: parse C++ source and return the ParseResult.
    fn parse_cpp(source: &str) -> ParseResult {
        parse_file("test.cpp", source)
    }

    // ─── Test 3.1: Basic function extraction ──────────────────────────────

    /// Parse a simple free C++ function — verify name, kind, and line numbers.
    #[test]
    fn test_basic_function_extraction() {
        let src = r#"
int add(int a, int b) {
    return a + b;
}
"#;
        let result = parse_cpp(src);
        let syms = &result.symbols;
        assert!(!syms.is_empty(), "expected at least one symbol");
        let func = syms.iter().find(|s| s.qualified.name == "add");
        assert!(func.is_some(), "expected symbol named 'add'; got: {:?}", syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>());
        let func = func.unwrap();
        assert_eq!(func.kind, SymbolKind::Function, "add must be Function kind");
        assert_eq!(func.qualified.scope_path, Vec::<String>::new(), "add must have empty scope_path at file level");
        assert!(func.line_start >= 1, "line_start must be >= 1");
        assert!(func.line_end >= func.line_start, "line_end must be >= line_start");
    }

    // ─── Test 3.2: Nested namespace scope_path ────────────────────────────

    /// Nested namespaces produce correct scope_path on the inner function.
    #[test]
    fn test_nested_namespace_scope_path() {
        let src = r#"
namespace outer {
    namespace inner {
        void foo() {}
    }
}
"#;
        let result = parse_cpp(src);
        let syms = &result.symbols;
        let foo = syms.iter().find(|s| s.qualified.name == "foo");
        assert!(foo.is_some(), "expected symbol 'foo'; got: {:?}", syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>());
        let foo = foo.unwrap();
        // scope_path should contain ["outer", "inner"] (the namespace names pushed by namespace_definition)
        assert!(
            foo.qualified.scope_path.contains(&"outer".to_string()),
            "scope_path must contain 'outer'; got: {:?}", foo.qualified.scope_path
        );
        assert!(
            foo.qualified.scope_path.contains(&"inner".to_string()),
            "scope_path must contain 'inner'; got: {:?}", foo.qualified.scope_path
        );
    }

    // ─── Test 3.3: Class with inline and out-of-line methods ──────────────

    /// Class with an inline method and an out-of-line `Foo::bar()` definition
    /// both produce Method-kind symbols.
    #[test]
    fn test_class_inline_and_outofline_methods() {
        let src = r#"
class Foo {
public:
    void inline_method() {}
    void bar();
};

void Foo::bar() {
    // out-of-line
}
"#;
        let result = parse_cpp(src);
        let syms = &result.symbols;

        // inline_method inside the class should be Method
        let inline_m = syms.iter().find(|s| s.qualified.name == "inline_method");
        assert!(inline_m.is_some(), "expected 'inline_method'; symbols: {:?}", syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>());
        assert_eq!(inline_m.unwrap().kind, SymbolKind::Method, "inline_method must be Method");

        // out-of-line Foo::bar() must also be Method
        let bar = syms.iter().find(|s| s.qualified.name == "bar");
        assert!(bar.is_some(), "expected 'bar'; symbols: {:?}", syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>());
        assert_eq!(bar.unwrap().kind, SymbolKind::Method, "Foo::bar must be Method (out-of-line)");
    }

    // ─── Test 3.4: Call edge from A to B ──────────────────────────────────

    /// When function A calls function B, a RawEdge must be produced.
    #[test]
    fn test_call_edge_a_calls_b() {
        let src = r#"
void b_func() {}

void a_func() {
    b_func();
}
"#;
        let result = parse_cpp(src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                e.from.name == "a_func" && name == "b_func"
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected edge from a_func to b_func; edges: {:?}",
            result.edges.iter().map(|e| (&e.from.name, &e.to)).collect::<Vec<_>>()
        );
        let edge = edge.unwrap();
        assert_eq!(edge.kind, EdgeKind::Calls, "edge kind must be Calls");
    }

    // ─── Test 3.5: #include extraction into imports HashMap ───────────────

    /// Both `#include <linux/list.h>` and `#include "local.h"` must produce
    /// entries in ParseResult.imports with the stripped path as key.
    #[test]
    fn test_include_extraction() {
        let src = r#"
#include <linux/list.h>
#include "local.h"

void foo() {}
"#;
        let result = parse_cpp(src);
        let imp = &result.imports;
        assert!(
            imp.contains_key("linux/list.h"),
            "imports must contain 'linux/list.h'; got: {:?}", imp.keys().collect::<Vec<_>>()
        );
        assert!(
            imp.contains_key("local.h"),
            "imports must contain 'local.h'; got: {:?}", imp.keys().collect::<Vec<_>>()
        );
    }

    // ─── Test 3.6: Import context propagation on qualified call ───────────

    /// `#include "Agent.h"` + `Agent::run()` call must produce an edge with
    /// `import_path = Some("Agent.h")`.
    #[test]
    fn test_import_context_propagation_qualified_call() {
        let src = r#"
#include "Agent.h"

void caller() {
    Agent::run();
}
"#;
        let result = parse_cpp(src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name == "run"
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected edge with callee 'run'; edges: {:?}",
            result.edges.iter().map(|e| &e.to).collect::<Vec<_>>()
        );
        let edge = edge.unwrap();
        if let EdgeTarget::Unresolved { import_path, .. } = &edge.to {
            assert_eq!(
                *import_path,
                Some("Agent.h".to_string()),
                "edge.import_path must be Some(\"Agent.h\")"
            );
        } else {
            panic!("edge must be Unresolved");
        }
    }

    // ─── Test 3.7: Qualified and field-expression call leaf names ─────────

    /// Various call expression forms must produce edges with correct callee
    /// leaf names:
    ///   - `ns::Foo::method()` → callee = "method"
    ///   - `obj.method()` → callee = "method"
    ///   - `ptr->method()` → callee = "method"
    #[test]
    fn test_qualified_and_field_call_leaf_names() {
        let src = r#"
void caller() {
    ns::Foo::method();
    obj.field_method();
    ptr->ptr_method();
}
"#;
        let result = parse_cpp(src);
        let callee_names: Vec<&str> = result.edges.iter().filter_map(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                Some(name.as_str())
            } else {
                None
            }
        }).collect();

        assert!(
            callee_names.contains(&"method"),
            "expected callee 'method' (from ns::Foo::method()); got: {:?}", callee_names
        );
        assert!(
            callee_names.contains(&"field_method"),
            "expected callee 'field_method' (from obj.field_method()); got: {:?}", callee_names
        );
        assert!(
            callee_names.contains(&"ptr_method"),
            "expected callee 'ptr_method' (from ptr->ptr_method()); got: {:?}", callee_names
        );
    }
}
