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
    let mut escaped = String::with_capacity(input.len() + 16);
    for c in input.chars() {
        match c {
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

fn yaml_to_minijinja(yaml: &Yaml) -> minijinja::Value {
    match yaml {
        Yaml::String(s) => minijinja::Value::from(s.clone()),
        Yaml::Integer(i) => minijinja::Value::from(*i),
        Yaml::Real(s) => s
            .parse::<f64>()
            .map(minijinja::Value::from)
            .unwrap_or_else(|_| minijinja::Value::from(s.clone())),
        Yaml::Boolean(b) => minijinja::Value::from(*b),
        Yaml::Array(a) => {
            let vec: Vec<_> = a.iter().map(yaml_to_minijinja).collect();
            minijinja::Value::from(vec)
        }
        Yaml::Hash(h) => {
            let mut map = BTreeMap::new();
            for (k, v) in h {
                if let Yaml::String(k_str) = k {
                    map.insert(k_str.clone(), yaml_to_minijinja(v));
                }
            }
            minijinja::Value::from(map)
        }
        Yaml::Null => minijinja::Value::from(()),
        _ => minijinja::Value::from(()),
    }
}

pub fn parse_markdown(content: &str) -> (BTreeMap<String, minijinja::Value>, String) {
    let mut meta = BTreeMap::new();
    meta.insert("title".to_string(), minijinja::Value::from("Lumen Page"));

    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let mut body = content;

    if content.starts_with("---\n") || content.starts_with("---\r\n") {
        let after_start = if content.starts_with("---\r\n") { 5 } else { 4 };
        if let Some(end_idx) = content[after_start..].find("\n---") {
            let fm_str = &content[after_start..after_start + end_idx];

            if let Ok(docs) = YamlLoader::load_from_str(fm_str) {
                if let Some(doc) = docs.first() {
                    if let Yaml::Hash(hash) = doc {
                        for (k, v) in hash {
                            if let Yaml::String(k_str) = k {
                                meta.insert(k_str.clone(), yaml_to_minijinja(v));
                            }
                        }
                    }
                }
            }

            let remainder = &content[after_start + end_idx..];
            if remainder.starts_with("\n---\r\n") {
                body = &remainder[6..];
            } else if remainder.starts_with("\n---\n") {
                body = &remainder[5..];
            } else {
                body = &remainder[4..];
            }

            body = body.trim_start();
        }
    }

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

    (meta, html_buf)
}

pub fn get_mime_type(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_secure_join_valid_paths() {
        let base = Path::new("/var/www/content");

        let res = secure_join(base, "index.md").unwrap();
        assert_eq!(res, base.join("index.md"));

        let res = secure_join(base, "posts/2024/hello.md").unwrap();
        assert_eq!(res, base.join("posts/2024/hello.md"));
    }

    #[test]
    fn test_secure_join_directory_traversal_attempts() {
        let base = Path::new("/var/www/content");

        let res = secure_join(base, "../../../etc/passwd");
        assert_eq!(res, None);

        let res = secure_join(base, "/etc/shadow").unwrap();
        assert_eq!(res, base.join("etc/shadow"));

        let res = secure_join(base, "posts/../index.md").unwrap();
        assert_eq!(res, base.join("index.md"));
    }

    #[test]
    fn test_escape_html_xss_payloads() {
        let payload = r#"<script>alert("XSS & 'stuff'")</script>"#;
        let escaped = escape_html(payload);
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(&quot;XSS &amp; &#x27;stuff&#x27;&quot;)&lt;/script&gt;"
        );

        let benign = "Just a normal string";
        assert_eq!(escape_html(benign), benign);
    }

    #[test]
    fn test_parse_markdown_frontmatter_edge_cases() {
        let windows_md = "---\r\ntitle: Windows\r\n---\r\n# Hello";
        let (meta1, html1) = parse_markdown(windows_md);
        assert_eq!(meta1.get("title").unwrap().to_string(), "Windows");
        assert!(html1.contains("<h1>Hello</h1>"));

        let no_fm = "# Just a heading";
        let (meta2, html2) = parse_markdown(no_fm);
        assert_eq!(meta2.get("title").unwrap().to_string(), "Lumen Page");
        assert!(html2.contains("<h1>Just a heading</h1>"));

        let bad_yaml = "---\ntitle:[Unclosed Array\n---\n# Content";
        let (meta3, html3) = parse_markdown(bad_yaml);
        assert_eq!(meta3.get("title").unwrap().to_string(), "Lumen Page");
        assert!(html3.contains("<h1>Content</h1>"));

        let bom_md = "\u{FEFF}---\ntitle: BOM\n---\nText";
        let (meta4, html4) = parse_markdown(bom_md);
        assert_eq!(meta4.get("title").unwrap().to_string(), "BOM");
        assert!(html4.contains("<p>Text</p>"));
    }
}
