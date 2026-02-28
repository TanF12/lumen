use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub paths: PathConfig,
    pub security: SecurityConfig,
    pub performance: PerformanceConfig,
}

#[derive(Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub threads: usize,
    pub queue_size: usize,
    pub read_timeout_secs: u64,
    pub write_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            name: "Lumen/1.0".into(),
            threads: 32,
            queue_size: 2000,
            read_timeout_secs: 10,
            write_timeout_secs: 15,
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct PathConfig {
    pub content_dir: String,
    pub theme_file: String,
    pub fallback_404: String,
}

impl Default for PathConfig {
    fn default() -> Self {
        Self {
            content_dir: "content".into(),
            theme_file: "themes/default/index.html".into(),
            fallback_404: "404".into(),
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct SecurityConfig {
    pub x_frame_options: String,
    pub x_content_type_options: String,
    pub content_security_policy: String,
    pub cors_allow_origin: String,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            x_frame_options: "DENY".into(),
            x_content_type_options: "nosniff".into(),
            content_security_policy:
                "default-src 'self'; style-src 'self' 'unsafe-inline'; media-src 'self'".into(),
            cors_allow_origin: "*".into(),
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct PerformanceConfig {
    pub connection_buffer_size: usize,
    pub enable_caching: bool,
    pub max_cache_items: usize,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            connection_buffer_size: 65536,
            enable_caching: true,
            max_cache_items: 1024,
        }
    }
}

pub fn load_config(path: &str) -> Config {
    if Path::new(path).exists() {
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("CRITICAL: Failed to parse config file '{}': {}", path, e);
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("CRITICAL: Failed to read config file '{}': {}", path, e);
                std::process::exit(1);
            }
        }
    } else {
        println!("No config found at {}. Using defaults.", path);
        Config::default()
    }
}
