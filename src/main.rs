use clap::{
    Parser, ValueEnum,
    builder::styling::{AnsiColor, Effects, Styles},
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::fs;

fn cli_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Green.on_default() | Effects::BOLD)
        .usage(AnsiColor::Magenta.on_default() | Effects::BOLD)
        .literal(AnsiColor::Cyan.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Yellow.on_default())
        .error(AnsiColor::Red.on_default() | Effects::BOLD)
        .valid(AnsiColor::Green.on_default() | Effects::BOLD)
        .invalid(AnsiColor::Yellow.on_default() | Effects::BOLD)
}

#[derive(Parser, Debug)]
#[command(name = "skelta")]
#[command(about = "A fast code structural blueprinter", long_about = None)]
#[command(styles = cli_styles())]
struct Cli {
    #[arg(default_value = "./")]
    path: String,
    #[arg(long, value_enum, default_value_t = Format::AgentMd)]
    format: Format,
    #[arg(long)]
    out: Option<String>,
    #[arg(long)]
    exclude: Vec<String>,
    #[arg(long)]
    focus: Option<String>,
    #[arg(long, default_value_t = 2)]
    depth_outside: usize,
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
enum Format {
    AgentMd,
    AgentJson,
    TreeOnly,
}

#[derive(Serialize, Clone)]
struct AgentJsonFile {
    is_focused: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    signatures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<String>,
}

#[derive(Serialize)]
struct ProjectMetadata {
    scan_directory: String,
    focused_path: Option<String>,
    total_files: usize,
    total_focused_files: usize,
}

#[derive(Serialize)]
struct AgentJsonOutput {
    project_metadata: ProjectMetadata,
    files: BTreeMap<String, AgentJsonFile>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    local_dependency_graph: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    sha256_hash: String,
    last_modified_timestamp: u64,
    signatures: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mut override_builder = OverrideBuilder::new(&cli.path);
    for exclude in &cli.exclude {
        let _ = override_builder.add(&format!("!{}", exclude));
    }
    let overrides = override_builder
        .build()
        .unwrap_or_else(|_| OverrideBuilder::new(&cli.path).build().unwrap());

    let output_tree = Arc::new(Mutex::new(String::new()));
    let output_files = Arc::new(Mutex::new(BTreeMap::new()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(64));

    let focus_path = cli
        .focus
        .as_ref()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| Path::new(p).to_path_buf()));
    let depth_outside = cli.depth_outside;

    let focus_path_filter = focus_path.clone();
    let walker = WalkBuilder::new(&cli.path)
        .overrides(overrides)
        .filter_entry(move |e| {
            if let Some(focus) = &focus_path_filter {
                let p = std::fs::canonicalize(e.path()).unwrap_or_else(|_| e.path().to_path_buf());
                if p.starts_with(focus) || focus.starts_with(&p) {
                    return true;
                }
                if e.depth() <= depth_outside {
                    return true;
                }
                false
            } else {
                true
            }
        })
        .build();

    let path_meta = std::fs::metadata(&cli.path).ok();
    let cache_dir = if path_meta.as_ref().is_some_and(|m| m.is_dir()) {
        Path::new(&cli.path).to_path_buf()
    } else if let Some(parent) = Path::new(&cli.path).parent() {
        parent.to_path_buf()
    } else {
        Path::new(".").to_path_buf()
    };

    let cache_file_path = cache_dir.join(".skelta_cache.json");
    let cache_data: HashMap<String, CacheEntry> = std::fs::read_to_string(&cache_file_path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();
    let cache_arc = Arc::new(Mutex::new(cache_data));

    let mut tasks = vec![];

    for entry in walker.flatten() {
        let format = cli.format.clone();
        if format == Format::TreeOnly {
            let depth = entry.depth();
            let name = entry.file_name().to_string_lossy().to_string();
            let indent = "  ".repeat(if depth > 0 { depth - 1 } else { 0 });
            let prefix = if depth > 0 { "├── " } else { "" };
            let mut out_md = output_tree.lock().unwrap();
            out_md.push_str(&format!("{}{}{}\n", indent, prefix, name));
            continue;
        }

        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let path = entry.path().to_path_buf();
            if !is_code_file(&path) {
                continue;
            }

            let is_focused = if let Some(focus) = &focus_path {
                let p = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                p.starts_with(focus)
            } else {
                true
            };

            let out_files_clone = Arc::clone(&output_files);
            let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
            let cache_arc_clone = Arc::clone(&cache_arc);

            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                let path_str = path.display().to_string();
                let mut signatures = Vec::new();
                let mut dependencies = Vec::new();

                if is_focused {
                    let metadata = fs::metadata(&path).await.ok();
                    let modified = metadata
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    let mut use_cache = false;
                    let mut cached_sigs = None;
                    let mut cached_deps = None;
                    let mut cached_hash = String::new();

                    {
                        let cache_lock = cache_arc_clone.lock().unwrap();
                        if let Some(entry) = cache_lock.get(&path_str) {
                            cached_hash = entry.sha256_hash.clone();
                            if entry.last_modified_timestamp == modified && modified > 0 {
                                use_cache = true;
                                cached_sigs = Some(entry.signatures.clone());
                                cached_deps = Some(entry.dependencies.clone());
                            }
                        }
                    }

                    let mut file_hash = String::new();
                    let (sigs, deps) = if use_cache {
                        (cached_sigs.unwrap(), cached_deps.unwrap_or_default())
                    } else if let Ok(content) = fs::read_to_string(&path).await {
                        let mut hasher = Sha256::new();
                        hasher.update(content.as_bytes());
                        file_hash = format!("{:x}", hasher.finalize());

                        if file_hash == cached_hash && !cached_hash.is_empty() {
                            let cache_lock = cache_arc_clone.lock().unwrap();
                            let entry = cache_lock.get(&path_str).unwrap();
                            (entry.signatures.clone(), entry.dependencies.clone())
                        } else {
                            process_file(&path_str, &content)
                        }
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    signatures = sigs;
                    dependencies = deps;

                    if !file_hash.is_empty() {
                        let mut cache_lock = cache_arc_clone.lock().unwrap();
                        cache_lock.insert(
                            path_str.clone(),
                            CacheEntry {
                                sha256_hash: file_hash,
                                last_modified_timestamp: modified,
                                signatures: signatures.clone(),
                                dependencies: dependencies.clone(),
                            },
                        );
                    } else if use_cache {
                        let mut cache_lock = cache_arc_clone.lock().unwrap();
                        if let Some(entry) = cache_lock.get_mut(&path_str) {
                            entry.last_modified_timestamp = modified;
                        }
                    }
                }

                let mut out_lock = out_files_clone.lock().unwrap();
                out_lock.insert(
                    path_str,
                    AgentJsonFile {
                        is_focused,
                        signatures,
                        dependencies,
                    },
                );
            }));
        }
    }

    futures_util::future::join_all(tasks).await;

    if let Ok(cache_lock) = cache_arc.lock() {
        let _ =
            serde_json::to_string(&*cache_lock).map(|json| std::fs::write(&cache_file_path, json));
    }

    // Build local dependency graph
    let (local_graph, graph_mermaid) = {
        let files = output_files.lock().unwrap();
        let mut edges = Vec::new();
        let mut mermaid = String::from("\n## Local Dependency Graph\n```mermaid\ngraph TD\n");
        for (file_path, file_data) in files.iter() {
            let file_name = Path::new(file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(file_path);
            for dep in &file_data.dependencies {
                for target_path in files.keys() {
                    let target_name = Path::new(target_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(target_path);
                    if dep.contains(target_name) && file_path != target_path {
                        edges.push((file_path.clone(), target_path.clone()));
                        let safe_src = file_name.replace(['-', '.'], "_");
                        let safe_tgt = target_name.replace(['-', '.'], "_");
                        mermaid.push_str(&format!(
                            "  {}[\"{}\"] --> {}[\"{}\"]\n",
                            safe_src, file_name, safe_tgt, target_name
                        ));
                        break;
                    }
                }
            }
        }
        mermaid.push_str("```\n");
        (edges, mermaid)
    };

    let mut out: Box<dyn Write> = match cli.out {
        Some(ref p) => {
            if let Some(parent) = std::path::Path::new(p).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Box::new(std::fs::File::create(p).expect("Failed to create output file"))
        }
        None => Box::new(io::stdout()),
    };

    if cli.format == Format::AgentMd {
        let files = output_files.lock().unwrap();
        let total_files = files.len();
        let total_focused = files.values().filter(|f| f.is_focused).count();

        let mut md = String::new();
        md.push_str("<!-- NOTE FOR AI: This is a structured, context-optimized codebase blueprint of the project.\n");
        md.push_str("     Method/function bodies are stripped to save tokens. Use this to understand code architecture,\n");
        md.push_str("     imports, and signatures. -->\n\n");
        md.push_str("# Project Codebase Blueprint\n\n");

        md.push_str("## Project Summary\n");
        md.push_str(&format!("* **Scan Directory:** `{}`\n", cli.path));
        md.push_str(&format!(
            "* **Focused Path:** `{}`\n",
            cli.focus.as_deref().unwrap_or("None")
        ));
        md.push_str(&format!("* **Total Files Scanned:** `{}`\n", total_files));
        md.push_str(&format!(
            "* **Total Focused Files:** `{}`\n\n",
            total_focused
        ));

        md.push_str("| File Path | Focus | Signatures | Dependencies |\n");
        md.push_str("|---|---|---|---|\n");
        for (path, file) in files.iter() {
            let focus_str = if file.is_focused {
                "**Focused**"
            } else {
                "Out of Focus"
            };
            md.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                path,
                focus_str,
                file.signatures.len(),
                file.dependencies.len()
            ));
        }

        if !local_graph.is_empty() {
            md.push_str(&graph_mermaid);
        }

        md.push_str("\n## Codebase Structural Blueprint\n");
        for (path, file) in files.iter() {
            if file.is_focused && (!file.signatures.is_empty() || !file.dependencies.is_empty()) {
                md.push_str(&format!("\n### [FOCUSED] File: {}\n", path));
                if !file.dependencies.is_empty() {
                    md.push_str("#### Dependencies:\n");
                    for dep in &file.dependencies {
                        md.push_str(&format!("- `{}`\n", dep));
                    }
                }
                if !file.signatures.is_empty() {
                    md.push_str("#### Signatures:\n");
                    for sig in &file.signatures {
                        md.push_str(&format!("- `{}`\n", sig));
                    }
                }
            } else if !file.is_focused {
                md.push_str(&format!("- [Out of focus] {}\n", path));
            }
        }
        write!(out, "{}", md).unwrap();
    } else if cli.format == Format::TreeOnly {
        let tree = output_tree.lock().unwrap();
        write!(out, "{}", tree).unwrap();
    } else if cli.format == Format::AgentJson {
        let files = output_files.lock().unwrap();
        let total_files = files.len();
        let total_focused = files.values().filter(|f| f.is_focused).count();

        let wrapper = AgentJsonOutput {
            project_metadata: ProjectMetadata {
                scan_directory: cli.path.clone(),
                focused_path: cli.focus.clone(),
                total_files,
                total_focused_files: total_focused,
            },
            files: files.clone(),
            local_dependency_graph: local_graph,
        };
        let json_str = serde_json::to_string_pretty(&wrapper).unwrap();
        writeln!(out, "{}", json_str).unwrap();
    }
}

fn is_code_file(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        matches!(
            ext.to_lowercase().as_str(),
            "rs" | "js"
                | "ts"
                | "jsx"
                | "tsx"
                | "py"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "h"
                | "hpp"
                | "cs"
                | "rb"
                | "php"
                | "swift"
                | "kt"
                | "scala"
                | "m"
                | "sh"
                | "bat"
                | "ps1"
        )
    } else {
        false
    }
}

fn process_file(path: &str, content: &str) -> (Vec<String>, Vec<String>) {
    let mut signatures = Vec::new();
    let mut dependencies = Vec::new();
    if path.ends_with(".rs") {
        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .is_ok()
        {
            let tree = parser
                .parse(content, None)
                .unwrap_or_else(|| panic!("Failed to parse Rust: {}", path));
            let mut cursor = tree.walk();
            extract_rust_sigs(
                content.as_bytes(),
                &mut cursor,
                &mut signatures,
                &mut dependencies,
            );
            return (signatures, dependencies);
        }
    } else if path.ends_with(".py") {
        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .is_ok()
        {
            let tree = parser
                .parse(content, None)
                .unwrap_or_else(|| panic!("Failed to parse Python: {}", path));
            let mut cursor = tree.walk();
            extract_python_sigs(
                content.as_bytes(),
                &mut cursor,
                &mut signatures,
                &mut dependencies,
            );
            return (signatures, dependencies);
        }
    } else if path.ends_with(".go") {
        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .is_ok()
        {
            let tree = parser
                .parse(content, None)
                .unwrap_or_else(|| panic!("Failed to parse Go: {}", path));
            let mut cursor = tree.walk();
            extract_go_sigs(
                content.as_bytes(),
                &mut cursor,
                &mut signatures,
                &mut dependencies,
            );
            return (signatures, dependencies);
        }
    } else if path.ends_with(".js")
        || path.ends_with(".jsx")
        || path.ends_with(".ts")
        || path.ends_with(".tsx")
    {
        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .is_ok()
        {
            let tree = parser
                .parse(content, None)
                .unwrap_or_else(|| panic!("Failed to parse JS: {}", path));
            let mut cursor = tree.walk();
            extract_js_sigs(
                content.as_bytes(),
                &mut cursor,
                &mut signatures,
                &mut dependencies,
            );
            return (signatures, dependencies);
        }
    }
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("import ")
            || trimmed.starts_with("require(")
            || trimmed.starts_with("from ")
        {
            dependencies.push(trimmed.to_string());
            continue;
        }
        let mut cleaned = trimmed;
        let mut changed = true;
        while changed {
            changed = false;
            for modifier in &[
                "export ",
                "default ",
                "async ",
                "public ",
                "private ",
                "protected ",
                "static ",
                "abstract ",
                "final ",
            ] {
                if cleaned.starts_with(modifier) {
                    cleaned = &cleaned[modifier.len()..];
                    changed = true;
                }
            }
        }
        let is_structural = [
            "fn ",
            "func ",
            "function ",
            "fun ",
            "class ",
            "struct ",
            "def ",
            "type ",
            "interface ",
        ]
        .iter()
        .any(|&k| cleaned.starts_with(k));
        if is_structural && trimmed.contains("{") {
            let sig = trimmed.split('{').next().unwrap().trim().to_string();
            signatures.push(sig);
        }
    }
    (signatures, dependencies)
}

fn extract_rust_sigs(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    signatures: &mut Vec<String>,
    dependencies: &mut Vec<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        if kind == "use_declaration" {
            if let Ok(dep) = std::str::from_utf8(&content[node.start_byte()..node.end_byte()]) {
                dependencies.push(dep.trim().to_string());
            }
        } else if kind == "function_item"
            || kind == "struct_item"
            || kind == "impl_item"
            || kind == "trait_item"
        {
            let mut sig_end = node.end_byte();
            if let Some(block) = node.child_by_field_name("body") {
                sig_end = block.start_byte();
            }
            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..sig_end]) {
                signatures.push(sig.trim().to_string());
            }
        }
        if cursor.goto_first_child() {
            extract_rust_sigs(content, cursor, signatures, dependencies);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn extract_python_sigs(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    signatures: &mut Vec<String>,
    dependencies: &mut Vec<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        if kind == "import_statement" || kind == "import_from_statement" {
            if let Ok(dep) = std::str::from_utf8(&content[node.start_byte()..node.end_byte()]) {
                dependencies.push(dep.trim().to_string());
            }
        } else if kind == "function_definition" || kind == "class_definition" {
            let mut sig_end = node.end_byte();
            if let Some(body) = node.child_by_field_name("body") {
                sig_end = body.start_byte();
            }
            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..sig_end]) {
                signatures.push(sig.trim().to_string());
            }
        }
        if cursor.goto_first_child() {
            extract_python_sigs(content, cursor, signatures, dependencies);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn extract_go_sigs(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    signatures: &mut Vec<String>,
    dependencies: &mut Vec<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        if kind == "import_declaration" {
            if let Ok(dep) = std::str::from_utf8(&content[node.start_byte()..node.end_byte()]) {
                dependencies.push(dep.trim().to_string());
            }
        } else if kind == "function_declaration" || kind == "method_declaration" {
            let mut sig_end = node.end_byte();
            if let Some(body) = node.child_by_field_name("body") {
                sig_end = body.start_byte();
            }
            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..sig_end]) {
                signatures.push(sig.trim().to_string());
            }
        } else if kind == "type_declaration" {
            let bytes = &content[node.start_byte()..node.end_byte()];
            if let Ok(sig) = std::str::from_utf8(bytes) {
                signatures.push(sig.trim().to_string());
            }
        }
        if cursor.goto_first_child() {
            extract_go_sigs(content, cursor, signatures, dependencies);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn extract_js_sigs(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    signatures: &mut Vec<String>,
    dependencies: &mut Vec<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        if kind == "import_statement" || kind == "export_statement" {
            if let Ok(dep) = std::str::from_utf8(&content[node.start_byte()..node.end_byte()]) {
                dependencies.push(dep.trim().to_string());
            }
        } else if kind == "function_declaration"
            || kind == "class_declaration"
            || kind == "method_definition"
        {
            let mut sig_end = node.end_byte();
            if let Some(body) = node.child_by_field_name("body") {
                sig_end = body.start_byte();
            }
            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..sig_end]) {
                signatures.push(sig.trim().to_string());
            }
        }
        if cursor.goto_first_child() {
            extract_js_sigs(content, cursor, signatures, dependencies);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}
