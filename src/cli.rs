use crate::{config::load_config, server::start_server};
use clap::{Parser, Subcommand};
use std::{fs, path::Path};

#[derive(Parser)]
#[command(
    name = "Lumen",
    version = "1.0.0",
    about = "Minimalist Markdown web server."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(default_value = ".")]
        path: String,
    },
    Start {
        #[arg(short, long)]
        port: Option<u16>,
        #[arg(short, long, default_value = "lumen.toml")]
        config: String,
        #[arg(long)]
        dev: bool,
    },
}

pub fn execute() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init { path } => {
            if let Err(e) = scaffold_workspace(&path) {
                eprintln!("ERROR: Failed to initialize workspace at '{}': {}", path, e);
                std::process::exit(1);
            }
        }
        Commands::Start { port, config, dev } => {
            let mut cfg = match load_config(&config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("CRITICAL: {}", e);
                    std::process::exit(1);
                }
            };

            if let Some(p) = port {
                cfg.server.port = p;
            }
            if dev {
                cfg.performance.enable_caching = false;
                println!("DEBUG: Developer mode enabled (Caching Disabled).");
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::DEBUG)
                    .init();
            } else {
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::INFO)
                    .init();
            }
            start_server(cfg);
        }
    }
}

fn scaffold_workspace(base_path: &str) -> std::io::Result<()> {
    let base = Path::new(base_path);
    fs::create_dir_all(base.join("content/posts"))?;
    fs::create_dir_all(base.join("themes/default"))?;

    let toml_path = base.join("lumen.toml");
    if !toml_path.exists() {
        fs::write(
            &toml_path,
            r#"[server]
host = "127.0.0.1"
port = 8080
name = "Lumen"
threads = 16
queue_size = 10000
timeout_secs = 15

[paths]
content_dir = "content"
theme_dir = "themes/default"
fallback_404 = "<h1>404 - File Not Found</h1>"

[security]
x_frame_options = "DENY"
x_content_type_options = "nosniff"
content_security_policy = "default-src 'self'; style-src 'self' 'unsafe-inline'; media-src 'self'"
cors_allow_origin = ""

[performance]
enable_caching = true
enable_compression = true
max_cache_memory_mb = 256
max_markdown_size_mb = 5
"#,
        )?;
    }

    let theme_path = base.join("themes/default/index.html");
    if !theme_path.exists() {
        fs::write(
            &theme_path,
            "<!DOCTYPE html>\n<html><head><title>{{ title }}</title></head><body>\n<main>\n<h1>{{ title }}</h1>\n{{ content|safe }}\n</main>\n</body></html>",
        )?;
    }

    let home_theme_path = base.join("themes/default/home.html");
    if !home_theme_path.exists() {
        fs::write(
            &home_theme_path,
            r#"<!DOCTYPE html>
<html>
<head><title>{{ title }}</title></head>
<body>
<main>
    <h1>{{ title }}</h1>
    {{ content|safe }}

    <h2>Recent Posts</h2>
    <ul>
    {% for post in list_dir("posts") %}
      <li><a href="{{ post.url }}">{{ post.title }}</a> - {{ post.date }}</li>
    {% endfor %}
    </ul>
</main>
</body>
</html>"#,
        )?;
    }

    let md_path = base.join("content/index.md");
    if !md_path.exists() {
        fs::write(
            &md_path,
            r#"---
title: "Welcome to Lumen"
template: "home.html"
cache: false
---
Server is running successfully!
"#,
        )?;
    }

    let post_path = base.join("content/posts/hello-world.md");
    if !post_path.exists() {
        fs::write(
            &post_path,
            "---\ntitle: \"Hello World\"\ndate: \"2026-03-01\"\n---\n\nThis is my first dynamic post via list_dir()!",
        )?;
    }

    println!(
        "Lumen workspace initialized at '{}'. Run `lumen start --dev` to begin.",
        base_path
    );
    Ok(())
}
