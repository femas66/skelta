use clap::{Parser, ValueEnum};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::fs;

#[derive(Parser, Debug)]
#[command(name = "skelta")]
#[command(about = "A fast code structural blueprinter", long_about = None)]
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
}

#[derive(ValueEnum, Clone, Debug, PartialEq)]
enum Format {
    AgentMd,
    AgentJson,
    TreeOnly,
}

#[derive(Serialize)]
struct AgentJsonOutput {
    files: BTreeMap<String, Vec<String>>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    
    let mut override_builder = OverrideBuilder::new(&cli.path);
    for exclude in &cli.exclude {
        let _ = override_builder.add(&format!("!{}", exclude));
    }
    let overrides = override_builder.build().unwrap_or_else(|_| OverrideBuilder::new(&cli.path).build().unwrap());

    let walker = WalkBuilder::new(&cli.path).overrides(overrides).build();

    let output_md = Arc::new(Mutex::new(String::new()));
    let output_json = Arc::new(Mutex::new(BTreeMap::new()));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(64)); // bound concurrency
    
    let mut tasks = vec![];

    for result in walker {
        if let Ok(entry) = result {
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

            if entry.file_type().map_or(false, |ft| ft.is_file()) {
                let path = entry.path().to_path_buf();
                
                if format != Format::TreeOnly && !is_code_file(&path) {
                    continue;
                }
                
                let out_md_clone = Arc::clone(&output_md);
                let out_json_clone = Arc::clone(&output_json);
                let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();

                tasks.push(tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(content) = fs::read_to_string(&path).await {
                        let path_str = path.display().to_string();
                        let signatures = process_file(&path_str, &content);
                        
                        if !signatures.is_empty() {
                            if format == Format::AgentMd {
                                let mut md = out_md_clone.lock().unwrap();
                                md.push_str(&format!("### File: {}\n", path_str));
                                for sig in signatures {
                                    md.push_str(&format!("- `{}`\n", sig));
                                }
                                md.push('\n');
                            } else if format == Format::AgentJson {
                                let mut json = out_json_clone.lock().unwrap();
                                json.insert(path_str, signatures);
                            }
                        }
                    }
                }));
            }
        }
    }

    futures_util::future::join_all(tasks).await;

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
        let wrapper = AgentJsonOutput { files: json.clone() };
        let json_str = serde_json::to_string_pretty(&wrapper).unwrap();
        writeln!(out, "{}", json_str).unwrap();
    }
}

// ponytail: basic extension filter
fn is_code_file(path: &PathBuf) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        matches!(
            ext.to_lowercase().as_str(),
            "rs" | "js" | "ts" | "jsx" | "tsx" | "py" | "go" | "java" | "c" | "cpp" | "h" | "hpp" | "cs" | "rb" | "php" | "swift" | "kt" | "scala" | "m" | "sh" | "bat" | "ps1"
        )
    } else {
        false
    }
}

// ponytail: tree-sitter integration for Rust. Other languages fallback to basic parser.
fn process_file(path: &str, content: &str) -> Vec<String> {
    if path.ends_with(".rs") {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into()).expect("Error loading Rust grammar");
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
            for modifier in &["export ", "default ", "async ", "public ", "private ", "protected ", "static ", "abstract ", "final "] {
                if cleaned.starts_with(modifier) {
                    cleaned = &cleaned[modifier.len()..];
                    changed = true;
                }
            }
        }

        let is_structural = ["fn ", "func ", "function ", "fun ", "class ", "struct ", "def ", "type ", "interface "]
            .iter()
            .any(|&k| cleaned.starts_with(k));

        if is_structural && trimmed.contains("{") {
            let sig = trimmed.split('{').next().unwrap().trim().to_string();
            signatures.push(sig);
        }
    }
    signatures
}

fn extract_rust_signatures(content: &[u8], cursor: &mut tree_sitter::TreeCursor, signatures: &mut Vec<String>) {
    loop {
        let node = cursor.node();
        let kind = node.kind();
        
        if kind == "function_item" || kind == "struct_item" || kind == "impl_item" || kind == "trait_item" {
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
