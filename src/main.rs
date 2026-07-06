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
    /// The directory to scan (defaults to current directory './')
    #[arg(default_value = "./")]
    path: String,

    /// Output format
    #[arg(long, value_enum, default_value_t = Format::AgentMd)]
    format: Format,

    /// Specific output file (defaults to stdout if not provided)
    #[arg(long)]
    out: Option<String>,

    /// Exclude specific file patterns (e.g., "*.md")
    #[arg(long)]
    exclude: Vec<String>,

    /// Specifies the file or directory that requires high-granularity extraction.
    #[arg(long)]
    focus: Option<String>,

    /// Specifies how deep the directory tree should be visualized for areas outside the focus window.
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
}

#[derive(Serialize)]
struct AgentJsonOutput {
    files: BTreeMap<String, AgentJsonFile>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CacheEntry {
    sha256_hash: String,
    last_modified_timestamp: u64,
    signatures: Vec<String>,
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

    let output_md = Arc::new(Mutex::new(String::new()));
    let output_json = Arc::new(Mutex::new(BTreeMap::new()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(64)); // bound concurrency

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

    // Load Cache
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
            let mut out_md = output_md.lock().unwrap();
            out_md.push_str(&format!("{}{}{}\n", indent, prefix, name));
            continue;
        }

        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let path = entry.path().to_path_buf();

            if format != Format::TreeOnly && !is_code_file(&path) {
                continue;
            }

            let is_focused = if let Some(focus) = &focus_path {
                let p = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                p.starts_with(focus)
            } else {
                true
            };

            let out_md_clone = Arc::clone(&output_md);
            let out_json_clone = Arc::clone(&output_json);
            let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
            let cache_arc_clone = Arc::clone(&cache_arc);

            tasks.push(tokio::spawn(async move {
                let _permit = permit;
                let path_str = path.display().to_string();

                let mut signatures = Vec::new();

                if is_focused {
                    let metadata = fs::metadata(&path).await.ok();
                    let modified = metadata
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    let mut use_cache = false;
                    let mut cached_sigs = None;
                    let mut cached_hash = String::new();

                    {
                        let cache_lock = cache_arc_clone.lock().unwrap();
                        if let Some(entry) = cache_lock.get(&path_str) {
                            cached_hash = entry.sha256_hash.clone();
                            if entry.last_modified_timestamp == modified && modified > 0 {
                                use_cache = true;
                                cached_sigs = Some(entry.signatures.clone());
                            }
                        }
                    }

                    let mut file_hash = String::new();
                    signatures = if use_cache {
                        cached_sigs.unwrap()
                    } else if let Ok(content) = fs::read_to_string(&path).await {
                        let mut hasher = Sha256::new();
                        hasher.update(content.as_bytes());
                        file_hash = format!("{:x}", hasher.finalize());

                        if file_hash == cached_hash && !cached_hash.is_empty() {
                            let cache_lock = cache_arc_clone.lock().unwrap();
                            cache_lock.get(&path_str).unwrap().signatures.clone()
                        } else {
                            process_file(&path_str, &content)
                        }
                    } else {
                        Vec::new()
                    };

                    if !file_hash.is_empty() {
                        let mut cache_lock = cache_arc_clone.lock().unwrap();
                        cache_lock.insert(
                            path_str.clone(),
                            CacheEntry {
                                sha256_hash: file_hash,
                                last_modified_timestamp: modified,
                                signatures: signatures.clone(),
                            },
                        );
                    } else if use_cache {
                        let mut cache_lock = cache_arc_clone.lock().unwrap();
                        if let Some(entry) = cache_lock.get_mut(&path_str) {
                            entry.last_modified_timestamp = modified;
                        }
                    }
                }

                if format == Format::AgentMd {
                    let mut md = out_md_clone.lock().unwrap();
                    if is_focused && !signatures.is_empty() {
                        md.push_str(&format!("\n### [FOCUSED] File: {}\n", path_str));
                        for sig in &signatures {
                            md.push_str(&format!("- `{}`\n", sig));
                        }
                    } else if !is_focused {
                        md.push_str(&format!("- [Out of focus] {}\n", path_str));
                    }
                } else if format == Format::AgentJson {
                    let mut json = out_json_clone.lock().unwrap();
                    json.insert(
                        path_str,
                        AgentJsonFile {
                            is_focused,
                            signatures,
                        },
                    );
                }
            }));
        }
    }

    futures_util::future::join_all(tasks).await;

    // Save cache
    if let Ok(cache_lock) = cache_arc.lock() {
        let _ =
            serde_json::to_string(&*cache_lock).map(|json| std::fs::write(&cache_file_path, json));
    }

    let mut out: Box<dyn Write> = match cli.out {
        Some(ref p) => {
            if let Some(parent) = std::path::Path::new(p).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Box::new(std::fs::File::create(p).expect("Failed to create output file"))
        }
        None => Box::new(io::stdout()),
    };

    if cli.format == Format::AgentMd || cli.format == Format::TreeOnly {
        let md = output_md.lock().unwrap();
        write!(out, "{}", md).unwrap();
    } else if cli.format == Format::AgentJson {
        let json = output_json.lock().unwrap();
        let wrapper = AgentJsonOutput {
            files: json.clone(),
        };
        let json_str = serde_json::to_string_pretty(&wrapper).unwrap();
        writeln!(out, "{}", json_str).unwrap();
    }
}

// ponytail: basic extension filter
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

// ponytail: tree-sitter integration for Rust. Other languages fallback to basic parser.
fn process_file(path: &str, content: &str) -> Vec<String> {
    if path.ends_with(".rs") {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("Error loading Rust grammar");
        let tree = parser.parse(content, None).unwrap();
        let mut cursor = tree.walk();

        let mut signatures = Vec::new();
        extract_rust_signatures(content.as_bytes(), &mut cursor, &mut signatures);
        return signatures;
    }

    // Fallback parser
    let mut signatures = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();

        // Strip common access/export modifiers in any order safely
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
    signatures
}

fn extract_rust_signatures(
    content: &[u8],
    cursor: &mut tree_sitter::TreeCursor,
    signatures: &mut Vec<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();

        if kind == "function_item"
            || kind == "struct_item"
            || kind == "impl_item"
            || kind == "trait_item"
        {
            let mut sig_end = node.end_byte();
            if let Some(block) = node.child_by_field_name("body") {
                sig_end = block.start_byte();
            } else {
                for i in 0..node.child_count() {
                    let child = node.child(i as u32).unwrap();
                    if child.kind() == "block" || child.kind() == "declaration_list" {
                        sig_end = child.start_byte();
                        break;
                    }
                }
            }

            if let Ok(sig) = std::str::from_utf8(&content[node.start_byte()..sig_end]) {
                signatures.push(sig.trim().to_string());
            }
        }

        if cursor.goto_first_child() {
            extract_rust_signatures(content, cursor, signatures);
            cursor.goto_parent();
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}
