use pulldown_cmark::{Options, Parser, html};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use yaml_rust2::{Yaml, YamlLoader};

#[inline(always)]
pub fn secure_join(base: &Path, user_path: &str) -> Option<PathBuf> {
    let mut result = base.to_path_buf();
    for component in Path::new(user_path).components() {
        match component {
            Component::Normal(c) => result.push(c),
            Component::ParentDir => {
                if result != base {
                    result.pop();
                } else {
                    return None;
                }
            }
            _ => continue, // automatically drops root dir overwrites
        }
    }
    Some(result)
}

#[inline(always)]
pub fn escape_html(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut escaped = Vec::with_capacity(bytes.len() + 16);
    for &b in bytes {
        match b {
            b'<' => escaped.extend_from_slice(b"&lt;"),
            b'>' => escaped.extend_from_slice(b"&gt;"),
            b'&' => escaped.extend_from_slice(b"&amp;"),
            b'"' => escaped.extend_from_slice(b"&quot;"),
            b'\'' => escaped.extend_from_slice(b"&#x27;"),
            _ => escaped.push(b),
        }
    }
    unsafe { String::from_utf8_unchecked(escaped) }
}

fn yaml_to_minijinja(yaml: Yaml) -> minijinja::Value {
    match yaml {
        Yaml::String(s) => minijinja::Value::from(s),
        Yaml::Integer(i) => minijinja::Value::from(i),
        Yaml::Real(s) => s
            .parse::<f64>()
            .map(minijinja::Value::from)
            .unwrap_or_else(|_| minijinja::Value::from(s)),
        Yaml::Boolean(b) => minijinja::Value::from(b),
        Yaml::Array(a) => {
            let vec: Vec<_> = a.into_iter().map(yaml_to_minijinja).collect();
            minijinja::Value::from(vec)
        }
        Yaml::Hash(h) => {
            let mut map = BTreeMap::new();
            for (k, v) in h {
                if let Yaml::String(k_str) = k {
                    map.insert(k_str, yaml_to_minijinja(v));
                }
            }
            minijinja::Value::from(map)
        }
        Yaml::Null => minijinja::Value::from(()),
        _ => minijinja::Value::from(()),
    }
}

pub fn split_frontmatter(content: &str) -> (BTreeMap<String, minijinja::Value>, &str) {
    let mut meta = BTreeMap::new();
    meta.insert("title".to_string(), minijinja::Value::from("Lumen Page"));

    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let mut body = content;

    let bytes = content.as_bytes();
    if bytes.starts_with(b"---\n") || bytes.starts_with(b"---\r\n") {
        let after_start = if bytes[3] == b'\r' { 5 } else { 4 };
        if let Some(end_idx) = content[after_start..].find("\n---") {
            let fm_str = &content[after_start..after_start + end_idx];

            if let Ok(mut docs) = YamlLoader::load_from_str(fm_str)
                && !docs.is_empty()
                && let Yaml::Hash(hash) = docs.remove(0)
            {
                for (k, v) in hash {
                    if let Yaml::String(k_str) = k {
                        meta.insert(k_str, yaml_to_minijinja(v));
                    }
                }
            }

            let remainder = &content[after_start + end_idx..];
            let rem_bytes = remainder.as_bytes();

            if rem_bytes.starts_with(b"\n---\r\n") {
                body = &remainder[6..];
            } else if rem_bytes.starts_with(b"\n---\n") {
                body = &remainder[5..];
            } else {
                body = &remainder[4..];
            }
        }
    }

    (meta, body.trim_start())
}

pub fn markdown_to_html(body: &str) -> String {
    let mut options = Options::empty();
    options.insert(
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_SMART_PUNCTUATION,
    );

    let parser = Parser::new_ext(body, options);
    let mut html_buf = String::with_capacity(body.len() * 2);
    html::push_html(&mut html_buf, parser);
    html_buf
}

pub fn get_mime_type(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}
