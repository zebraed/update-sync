# update-sync

A small CLI that performs **one-way sync** from a source directory to a target directory, based on file presence and last modified time (mtime).

## Background

This project is a **Rust port** of my old Python script (`update.py`) in this Gist:

- <https://gist.github.com/zebraed/17f07a8504dd733751c6f0f95fda08e5>

It was rewritten for **learning Rust**, with few dependencies and behavior that is easy to read and reason about. The goal is to stay **simple, lightweight, and clear**.

## Requirements

- A Rust toolchain that supports `edition = "2024"` (see `Cargo.toml`).

## Build and run

```bash
cargo build --release
cargo run --release -- <SOURCE> <TARGET>
```

Install from this repository:

```bash
cargo install --path .
```

## Usage

```text
update-sync <SOURCE> <TARGET>
```

By default: overwrite when the source file is newer, create missing files and directories, and delete extra files and directories on the target. For first runs or before applying changes, prefer **`--dry-run` (`-r`)**.

### Common options

| Option | Description |
|--------|-------------|
| `-r`, `--dry-run` | Print planned operations without making changes |
| `-w`, `--no-overwrite` | Do not overwrite even when the source is newer |
| `-n`, `--no-new` | Do not create new files or directories on the target |
| `-d`, `--no-delete` | Do not delete extra files or directories on the target |
| `-t`, `--time-tolerance-seconds` | mtime comparison tolerance in seconds (default: `1.0`) |
| `-i`, `--ignore <FILE>` | Exclude paths using gitignore-style rules from the given file (repeatable) |


## License

MIT — see `LICENSE`.
