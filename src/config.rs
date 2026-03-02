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
    pub timeout_secs: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8080,
            name: "Lumen".into(),
            threads: 32,
            queue_size: 10_000,
            timeout_secs: 15,
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct PathConfig {
    pub content_dir: String,
    pub theme_dir: String,
    pub fallback_404: String,
}
impl Default for PathConfig {
    fn default() -> Self {
        Self {
            content_dir: "content".into(),
            theme_dir: "themes/default".into(),
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
            cors_allow_origin: "".into(),
        }
    }
}

#[derive(Deserialize, Clone)]
pub struct PerformanceConfig {
    pub enable_caching: bool,
    pub enable_compression: bool,
    pub max_cache_memory_mb: usize,
    pub max_markdown_size_mb: usize,
}
impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            enable_caching: true,
            enable_compression: true,
            max_cache_memory_mb: 256,
            max_markdown_size_mb: 5,
        }
    }
}

pub fn load_config(path: &str) -> Result<Config, String> {
    if Path::new(path).exists() {
        match fs::read_to_string(path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(c) => Ok(c),
                Err(e) => Err(format!("Failed to parse config file: {}", e)),
            },
            Err(e) => Err(format!("Failed to read config file '{}': {}", path, e)),
        }
    } else {
        Ok(Config::default())
    }
}
