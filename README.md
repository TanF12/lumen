# Lumen

> A lightweight, minimalist, and secure Markdown web server written in Rust

Lumen is designed to serve Markdown files as rendered HTML pages with zero configuration. It features a custom work-stealing thread pool, and aggressive in-memory caching

## Features

- **Fast**: Custom thread-pool, zero-copy HTTP parsing, and sharded LRU caching.
- **Media Streaming**: Native support for HTTP `Range` requests (streams `.mp4`, `.mp3` out of the box).

## Getting Started

### Installation
Ensure you have [Rust installed](https://rustup.rs/), then clone and build:

```bash
git clone https://github.com/TanF12/lumen.git
cd lumen
cargo build --release
```

### Quick Start

1. Initialize a new workspace (creates `lumen.toml`, content, and theme directories):
   ```bash
   ./target/release/lumen init .
   ```
2. Start the server in developer mode (disables caching for live-reloading):
   ```bash
   ./target/release/lumen start --dev
   ```

## ⚙️ Configuration
Lumen is configured via `lumen.toml`. See the generated file for options regarding threading, timeouts, cache sizes, and security headers (CORS, CSP, etc.).