use anyhow::{Context, Result};
use clap::{
    Parser, ValueEnum,
    builder::styling::{AnsiColor, Effects, Styles},
};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
struct AgentJsonOutput<'a> {
    project_metadata: ProjectMetadata,
    files: &'a BTreeMap<String, AgentJsonFile>,
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
async fn main() -> Result<()> {
    let cli = Cli::try_parse()?;

    let mut override_builder = OverrideBuilder::new(&cli.path);
    for exclude in &cli.exclude {
        let _ = override_builder.add(&format!("!{}", exclude));
    }
    let overrides = override_builder
        .build()
        .or_else(|_| OverrideBuilder::new(&cli.path).build())
        .context("Failed to build ignore overrides")?;

    let semaphore = Arc::new(tokio::sync::Semaphore::new(64));

    let focus_path_canon_str = cli.focus.as_ref().map(|p| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| PathBuf::from(p))
            .to_string_lossy()
            .to_string()
    });

    let depth_outside = cli.depth_outside;
    let focus_path_filter = focus_path_canon_str.clone();
    let root_canon = std::fs::canonicalize(&cli.path).unwrap_or_else(|_| PathBuf::from(&cli.path));
    let root_canon_clone = root_canon.clone();
    let cli_path_str = cli.path.clone();

    let walker = WalkBuilder::new(&cli.path)
        .overrides(overrides)
        .filter_entry(move |e| {
            if let Some(focus) = &focus_path_filter {
                let rel = e.path().strip_prefix(&cli_path_str).unwrap_or(e.path());
                let p = root_canon_clone.join(rel).to_string_lossy().to_string();
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
    let cache_data: HashMap<String, CacheEntry> = tokio::fs::read_to_string(&cache_file_path)
        .await
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default();

    let cache_arc = Arc::new(cache_data);
    let mut tasks = vec![];
    let mut output_tree = String::new();

    for entry in walker.flatten() {
        let format = cli.format.clone();
        if format == Format::TreeOnly {
            let depth = entry.depth();
            let name = entry.file_name().to_string_lossy().to_string();
            let indent = "  ".repeat(if depth > 0 { depth - 1 } else { 0 });
            let prefix = if depth > 0 { "├── " } else { "" };
            output_tree.push_str(&format!("{}{}{}\n", indent, prefix, name));
            continue;
        }

        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let path = entry.path().to_path_buf();
            if !is_code_file(&path) {
                continue;
            }

            let path_str = path.display().to_string();
            let is_focused = if let Some(focus) = &focus_path_canon_str {
                let rel = path.strip_prefix(&cli.path).unwrap_or(&path);
                let p = root_canon.join(rel).to_string_lossy().to_string();
                p.starts_with(focus)
            } else {
                true
            };

            let permit = Arc::clone(&semaphore)
                .acquire_owned()
                .await
                .context("Failed to acquire semaphore permit")?;
            let cache_arc_clone = Arc::clone(&cache_arc);

            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                let mut signatures = Vec::new();
                let mut dependencies = Vec::new();
                let mut new_cache_entry: Option<CacheEntry> = None;

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

                    if let Some(entry) = cache_arc_clone.get(&path_str) {
                        cached_hash = entry.sha256_hash.clone();
                        if entry.last_modified_timestamp == modified && modified > 0 {
                            use_cache = true;
                            cached_sigs = Some(entry.signatures.clone());
                            cached_deps = Some(entry.dependencies.clone());
                        }
                    }

                    let mut file_hash = String::new();
                    let (sigs, deps) = if use_cache {
                        (
                            cached_sigs.unwrap_or_default(),
                            cached_deps.unwrap_or_default(),
                        )
                    } else if let Ok(content) = fs::read_to_string(&path).await {
                        let mut hasher = Sha256::new();
                        hasher.update(content.as_bytes());
                        file_hash = format!("{:x}", hasher.finalize());

                        if file_hash == cached_hash && !cached_hash.is_empty() {
                            if let Some(entry) = cache_arc_clone.get(&path_str) {
                                (entry.signatures.clone(), entry.dependencies.clone())
                            } else {
                                (Vec::new(), Vec::new())
                            }
                        } else {
                            let path_clone = path_str.clone();
                            tokio::task::spawn_blocking(move || process_file(&path_clone, &content))
                                .await
                                .unwrap_or_default()
                        }
                    } else {
                        (Vec::new(), Vec::new())
                    };

                    signatures = sigs.clone();
                    dependencies = deps.clone();

                    if !file_hash.is_empty() {
                        new_cache_entry = Some(CacheEntry {
                            sha256_hash: file_hash,
                            last_modified_timestamp: modified,
                            signatures: signatures.clone(),
                            dependencies: dependencies.clone(),
                        });
                    } else if use_cache {
                        if let Some(entry) = cache_arc_clone.get(&path_str) {
                            let mut updated_entry = entry.clone();
                            updated_entry.last_modified_timestamp = modified;
                            new_cache_entry = Some(updated_entry);
                        }
                    }
                }

                let file_data = AgentJsonFile {
                    is_focused,
                    signatures,
                    dependencies,
                };

                (path_str, file_data, new_cache_entry)
            }));
        }
    }

    let mut output_files = BTreeMap::new();
    let mut updated_cache = Arc::try_unwrap(cache_arc).unwrap_or_else(|arc| (*arc).clone());

    let results = futures_util::future::join_all(tasks).await;
    for (path_str, file_data, cache_entry_opt) in results.into_iter().flatten() {
        output_files.insert(path_str.clone(), file_data);
        if let Some(entry) = cache_entry_opt {
            updated_cache.insert(path_str, entry);
        }
    }

    if let Ok(json) = serde_json::to_string(&updated_cache) {
        let _ = tokio::fs::write(&cache_file_path, json).await;
    }

    let (local_graph, graph_mermaid) = {
        let mut edges = Vec::new();
        let mut mermaid = String::from("\n## Local Dependency Graph\n```mermaid\ngraph TD\n");
        let mut name_to_path = HashMap::new();
        let mut safe_names = HashMap::new();

        for file_path in output_files.keys() {
            let file_name = Path::new(file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(file_path);
            let name_str = file_name.to_string();
            name_to_path.insert(name_str.clone(), file_path.clone());
            safe_names.insert(name_str, file_name.replace(['-', '.'], "_"));
        }

        for (file_path, file_data) in &output_files {
            let file_name = Path::new(file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(file_path);
            let safe_src = safe_names.get(file_name).unwrap();

            let mut seen_deps = HashSet::new();

            for dep in &file_data.dependencies {
                for token in dep.split(|c: char| !c.is_alphanumeric()) {
                    if token.is_empty() {
                        continue;
                    }
                    if let Some(target_path) = name_to_path.get(token) {
                        if file_path != target_path && seen_deps.insert(target_path.clone()) {
                            edges.push((file_path.clone(), target_path.clone()));
                            let safe_tgt = safe_names.get(token).unwrap();
                            mermaid.push_str(&format!(
                                "  {}[\"{}\"] --> {}[\"{}\"]\n",
                                safe_src, file_name, safe_tgt, token
                            ));
                        }
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
            Box::new(std::fs::File::create(p).context("Failed to create output file")?)
        }
        None => Box::new(io::stdout()),
    };

    if cli.format == Format::AgentMd {
        let total_files = output_files.len();
        let total_focused = output_files.values().filter(|f| f.is_focused).count();

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
        for (path, file) in output_files.iter() {
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
        for (path, file) in output_files.iter() {
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
        write!(out, "{}", md).context("Failed to write to output")?;
    } else if cli.format == Format::TreeOnly {
        write!(out, "{}", output_tree).context("Failed to write tree output")?;
    } else if cli.format == Format::AgentJson {
        let total_files = output_files.len();
        let total_focused = output_files.values().filter(|f| f.is_focused).count();

        let wrapper = AgentJsonOutput {
            project_metadata: ProjectMetadata {
                scan_directory: cli.path.clone(),
                focused_path: cli.focus.clone(),
                total_files,
                total_focused_files: total_focused,
            },
            files: &output_files,
            local_dependency_graph: local_graph,
        };
        let json_str =
            serde_json::to_string_pretty(&wrapper).context("Failed to serialize output json")?;
        writeln!(out, "{}", json_str).context("Failed to write json output")?;
    }

    Ok(())
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

    let path_obj = Path::new(path);
    let ext = path_obj
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let mut parser = tree_sitter::Parser::new();
    let (lang, dep_types, sig_types_body, sig_types_no_body): (
        Option<tree_sitter::Language>,
        &[&str],
        &[&str],
        &[&str],
    ) = match ext.as_str() {
        "rs" => (
            Some(tree_sitter_rust::LANGUAGE.into()),
            &["use_declaration"],
            &["function_item", "struct_item", "impl_item", "trait_item"],
            &[],
        ),
        "py" => (
            Some(tree_sitter_python::LANGUAGE.into()),
            &["import_statement", "import_from_statement"],
            &["function_definition", "class_definition"],
            &[],
        ),
        "go" => (
            Some(tree_sitter_go::LANGUAGE.into()),
            &["import_declaration"],
            &["function_declaration", "method_declaration"],
            &["type_declaration"],
        ),
        "js" | "jsx" | "ts" | "tsx" => (
            Some(tree_sitter_javascript::LANGUAGE.into()),
            &["import_statement", "export_statement"],
            &[
                "function_declaration",
                "class_declaration",
                "method_definition",
            ],
            &[],
        ),
        _ => (None, &[], &[], &[]),
    };

    if let Some(language) = lang {
        if parser.set_language(&language).is_ok() {
            if let Some(tree) = parser.parse(content, None) {
                let mut cursor = tree.walk();
                extract_ast_nodes(
                    content.as_bytes(),
                    &mut cursor,
                    dep_types,
                    sig_types_body,
                    sig_types_no_body,
                    &mut signatures,
                    &mut dependencies,
                );
                return (signatures, dependencies);
            }
        }
    }

    // Fallback regex
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

        if is_structural {
            let sig = match trimmed.find('{') {
                Some(brace_idx) => trimmed[..brace_idx].trim(),
                None => trimmed,
            };
            if !sig.is_empty() {
                signatures.push(sig.to_string());
            }
        }
    }
    (signatures, dependencies)
}

fn extract_ast_nodes(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    dep_types: &[&str],
    sig_types_body: &[&str],
    sig_types_no_body: &[&str],
    signatures: &mut Vec<String>,
    dependencies: &mut Vec<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        if dep_types.contains(&kind) {
            if let Ok(dep) = std::str::from_utf8(&content[node.start_byte()..node.end_byte()]) {
                dependencies.push(dep.trim().to_string());
            }
        } else if sig_types_body.contains(&kind) {
            let mut sig_end = node.end_byte();
            if let Some(body) = node.child_by_field_name("body") {
                sig_end = body.start_byte();
            }
            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..sig_end]) {
                signatures.push(sig.trim().to_string());
            }
        } else if sig_types_no_body.contains(&kind) {
            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..node.end_byte()]) {
                signatures.push(sig.trim().to_string());
            }
        }
        if cursor.goto_first_child() {
            extract_ast_nodes(
                content,
                cursor,
                dep_types,
                sig_types_body,
                sig_types_no_body,
                signatures,
                dependencies,
            );
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}
