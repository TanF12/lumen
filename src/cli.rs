use crate::{config::load_config, server::start_server};
use clap::{Parser, Subcommand};
use std::{fs, path::Path};

#[derive(Parser)]
#[command(
    name = "Lumen",
    version = "1.0",
    author = "Eduardo J. Becker",
    about = "Minimalist Markdown web server"
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
        Commands::Init { path } => scaffold_workspace(&path),
        Commands::Start { port, config, dev } => {
            tracing_subscriber::fmt()
                .with_max_level(if dev {
                    tracing::Level::DEBUG
                } else {
                    tracing::Level::INFO
                })
                .init();

            let mut cfg = load_config(&config);
            if let Some(p) = port {
                cfg.server.port = p;
            }
            if dev {
                cfg.performance.enable_caching = false;
                tracing::debug!("Developer mode enabled: caching disabled.");
            }
            start_server(cfg);
        }
    }
}

fn scaffold_workspace(base_path: &str) {
    let base = Path::new(base_path);
    fs::create_dir_all(base.join("content")).unwrap();
    fs::create_dir_all(base.join("themes/default")).unwrap();

    let toml_path = base.join("lumen.toml");
    if !toml_path.exists() {
        fs::write(&toml_path, "[server]\nhost = \"0.0.0.0\"\nport = 8080\nname = \"Lumen/1.0\"\nthreads = 32\nqueue_size = 2000\nread_timeout_secs = 10\nwrite_timeout_secs = 15\n\n[paths]\ncontent_dir = \"content\"\ntheme_file = \"themes/default/index.html\"\nfallback_404 = \"<h1>404 - File Not Found</h1>\"\n\n[security]\nx_frame_options = \"DENY\"\nx_content_type_options = \"nosniff\"\ncontent_security_policy = \"default-src 'self'; style-src 'self' 'unsafe-inline'; media-src 'self'\"\ncors_allow_origin = \"*\"\n\n[performance]\nconnection_buffer_size = 65536\nenable_caching = true\nmax_cache_items = 1024\n").unwrap();
    }

    let theme_path = base.join("themes/default/index.html");
    if !theme_path.exists() {
        fs::write(&theme_path, "<!DOCTYPE html>\n<html><head><title>{{ title }}</title></head><body>\n<main>\n<h1>{{ title }}</h1>\n{{ content|safe }}\n</main>\n</body></html>").unwrap();
    }

    let md_path = base.join("content/index.md");
    if !md_path.exists() {
        fs::write(
            &md_path,
            "---\ntitle: \"Welcome to Lumen\"\n---\n\nServer is running successfully!",
        )
        .unwrap();
    }

    println!(
        "Lumen workspace initialized at '{}'. Run `lumen start --dev` to begin.",
        base_path
    );
}
