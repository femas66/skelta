# Skelta

Skelta is a lightning-fast, zero-bloat CLI tool built in Rust that flattens your entire project directory into a single, highly compressed context file (Markdown or JSON). It strips out raw internal logic (function/method bodies) and leaves behind only the structural blueprint (signatures, imports, and docstrings). 

It is specifically designed for **AI Coding Agents** (like Anthropic Computer Use, OpenClaw, etc.) to understand massive codebases instantly, reducing context token consumption by up to 95%.

## Why use Skelta?
- **Massive Token Savings:** Stop feeding AI agents 10,000 lines of implementation details when they only need to know how components interact. Skelta gives the AI the macro-architecture without the noise.
- **Universal AST Parsing:** Powered by `tree-sitter`, Skelta performs true syntactic analysis (starting natively with Rust) to cleanly extract structural blueprints without regex hacks.
- **Asynchronous Parallel Speed:** Built on top of `tokio`, Skelta dispatches file parsing across all available CPU cores concurrently while strictly managing memory limits via backpressure semaphores.
- **`.gitignore` Native:** Automatically respects your `.gitignore` rules (powered by the `ignore` crate) so `node_modules` and `target` directories never bloat your blueprint.

## How it works
1. **Discovery:** Traverses directories asynchronously, filtering out ignored files and non-coding file extensions.
2. **Parallel AST Parsing:** Dispatches syntax tree extraction to the `tokio` thread pool. `tree-sitter` analyzes the structure to identify classes, structs, and methods.
3. **Abstraction (Flattener):** Strips out *all* internal function logic completely. Only exact method/function signatures are retained in the final output format.

## Prerequisites
- [Rust](https://rustup.rs/) (cargo) installed on your system.

## Installation
Clone the repository and install it globally using `cargo`:

```bash
git clone https://github.com/femas66/skelta.git
cd skelta
cargo install --path .
```

*Alternatively, you can just run it directly from the source directory using `cargo run --release`.*

## Output Formats
- **`agent-md` (Default):** Hyper-dense Markdown optimized for LLM reading.
- **`agent-json`:** Structured JSON format for API-based AI systems.
- **`tree-only`:** Minimalist directory and file structure tree.

## Usage

Run `skelta` with a target directory (defaults to current directory if omitted). By default, the output is printed to `stdout`, making it easy to pipe to other tools.

```bash
# Basic usage (Outputs to stdout using agent-md)
skelta ./my-project

# Specific output format and saving to file
skelta ./backend --format agent-json --out blueprint.json

# Just print the file tree
skelta ./src --format tree-only

# Exclude specific patterns
skelta ./src --exclude "*.md"

# View all commands and flags
skelta --help
```

## License

This project is licensed under the MIT License.
