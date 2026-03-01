use httparse::Request;
use minijinja::Environment;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use std::{
    fs::{self, File},
    hash::Hasher,
    io::{Read, Seek, SeekFrom, Write},
    sync::Arc,
    time::SystemTime,
};
use tracing::error;

use crate::{
    server::LumenStream,
    state::{CacheEntry, FxHasher, ServerState},
    utils::{escape_html, get_mime_type, markdown_to_html, secure_join, split_frontmatter},
};

pub const PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

fn get_jinja_env(state: &Arc<ServerState>) -> Arc<Environment<'static>> {
    let theme_dir = &state.config.paths.theme_dir;

    let mut hasher = FxHasher::default();
    let mut max_mtime = SystemTime::UNIX_EPOCH;
    let mut file_count = 0usize;

    if let Ok(entries) = fs::read_dir(theme_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                file_count += 1;
                if let Ok(mtime) = meta.modified()
                    && mtime > max_mtime
                {
                    max_mtime = mtime;
                }
            }
        }
    }

    let mtime_secs = max_mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    hasher.write(&mtime_secs.to_ne_bytes());
    hasher.write(&file_count.to_ne_bytes());
    let current_hash = hasher.finish();

    {
        let cache = state.theme_state.read().unwrap_or_else(|e| e.into_inner());
        if cache.0 == current_hash {
            return Arc::clone(&cache.1);
        }
    }

    let mut cache = state.theme_state.write().unwrap_or_else(|e| e.into_inner());
    if cache.0 == current_hash {
        return Arc::clone(&cache.1);
    }

    let mut env = Environment::new();
    let env_state = Arc::clone(state);

    env.add_function("list_dir", move |dir_path: String| -> minijinja::Value {
        let target_dir = match secure_join(&env_state.base_dir, &dir_path) {
            Some(path) => path,
            None => return minijinja::Value::from(Vec::<minijinja::Value>::new()),
        };

        let mut dir_hash = 0u64;
        let mut file_entries = Vec::new();

        if let Ok(read_dir) = fs::read_dir(&target_dir) {
            for entry in read_dir.flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "md")
                    && let Ok(meta) = entry.metadata()
                {
                    let mtime = meta
                        .modified()
                        .unwrap_or(SystemTime::UNIX_EPOCH)
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    dir_hash = (dir_hash.rotate_left(3) ^ mtime).wrapping_add(meta.len());
                    file_entries.push(entry);
                }
            }
        }

        if let Some((cached_hash, cached_val)) = env_state.dir_cache.get(&target_dir)
            && cached_hash == dir_hash
        {
            return cached_val;
        }

        let mut entries = Vec::new();
        for entry in file_entries {
            let file_stem = entry
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let content = fs::read_to_string(entry.path()).unwrap_or_default();
            let (mut meta, _) = split_frontmatter(&content);

            let url = if file_stem == "index" {
                format!("/{}/", dir_path)
            } else {
                format!("/{}/{}", dir_path, file_stem)
            };

            meta.insert("url".to_string(), minijinja::Value::from(url));
            entries.push(minijinja::Value::from(meta));
        }

        entries.sort_by(|a, b| {
            let d1 = a.get_attr("date").unwrap_or_default().to_string();
            let d2 = b.get_attr("date").unwrap_or_default().to_string();
            d2.cmp(&d1)
        });

        let val = minijinja::Value::from(entries);
        env_state.dir_cache.put(target_dir, (dir_hash, val.clone()));
        val
    });

    if let Ok(entries) = fs::read_dir(theme_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                let name = entry.file_name().to_string_lossy().to_string();
                let content = fs::read_to_string(entry.path()).unwrap_or_default();
                let _ = env.add_template_owned(name.clone(), content.clone());

                if name == "index.html" {
                    let _ = env.add_template_owned("index".to_string(), content);
                }
            }
        }
    } else {
        let _ = env.add_template_owned("index", "{{ content|safe }}".to_string());
    }

    let arc_env = Arc::new(env);
    *cache = (current_hash, Arc::clone(&arc_env));

    if state.config.performance.enable_caching {
        state.page_cache.clear();
    }

    arc_env
}

fn serve_markdown(
    stream: &mut LumenStream,
    mut file: File,
    md_path: &std::path::Path,
    mtime: SystemTime,
    keep_alive: bool,
    state: &Arc<ServerState>,
) -> std::io::Result<(bool, u16)> {
    let cache_key = md_path.to_path_buf();

    if state.config.performance.enable_caching
        && let Some(entry) = state.page_cache.get(&cache_key)
        && entry.mtime == mtime
    {
        return send_response(
            stream,
            200,
            entry.html.as_bytes(),
            "text/html; charset=utf-8",
            keep_alive,
            state,
            None,
        );
    }

    let mut content = String::new();
    if file.read_to_string(&mut content).is_ok() {
        let (mut meta, raw_body) = split_frontmatter(&content);

        let use_cache = state.config.performance.enable_caching
            && meta
                .get("cache")
                .map(|v| {
                    if let Ok(b) = bool::try_from(v.clone()) {
                        b
                    } else if let Some(s) = v.as_str() {
                        s == "true"
                    } else {
                        true
                    }
                })
                .unwrap_or(true);

        let template_name = meta
            .get("template")
            .and_then(|v| v.as_str())
            .unwrap_or("index")
            .to_string();
        let content_type = meta
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or("text/html; charset=utf-8")
            .to_string();

        let env = get_jinja_env(state);

        let rendered_body = match env.render_str(raw_body, minijinja::Value::from(meta.clone())) {
            Ok(r) => r,
            Err(e) => {
                error!(
                    "Markdown template render error in {}: {}",
                    md_path.display(),
                    e
                );
                raw_body.to_string()
            }
        };

        let html_body = markdown_to_html(&rendered_body);
        meta.insert("content".to_string(), minijinja::Value::from(html_body));

        match env.get_template(&template_name) {
            Ok(template) => match template.render(minijinja::Value::from(meta)) {
                Ok(rendered) => {
                    let rendered_arc = Arc::new(rendered);
                    if use_cache {
                        state.page_cache.put(
                            cache_key,
                            CacheEntry {
                                html: Arc::clone(&rendered_arc),
                                mtime,
                            },
                        );
                    }
                    return send_response(
                        stream,
                        200,
                        rendered_arc.as_bytes(),
                        &content_type,
                        keep_alive,
                        state,
                        None,
                    );
                }
                Err(e) => {
                    error!("Theme render error: {}", e);
                    return send_error(stream, 500, b"Internal Server Error", keep_alive, state);
                }
            },
            Err(_) => {
                return send_error(stream, 500, b"Internal Server Error", keep_alive, state);
            }
        }
    }

    send_error(
        stream,
        404,
        state.config.paths.fallback_404.as_bytes(),
        keep_alive,
        state,
    )
}

pub fn serve_path(
    stream: &mut LumenStream,
    req_path: &str,
    headers: &[httparse::Header],
    state: &Arc<ServerState>,
    keep_alive: bool,
) -> std::io::Result<(bool, u16)> {
    let decoded_path = percent_decode_str(req_path)
        .decode_utf8()
        .unwrap_or_else(|_| req_path.into());
    let normalized = decoded_path
        .split('?')
        .next()
        .unwrap_or("/")
        .replace('\\', "/");

    if normalized.contains("..") || normalized.contains("/.") || normalized.starts_with('.') {
        return send_error(stream, 403, b"403 Forbidden", keep_alive, state);
    }

    let target = normalized.trim_start_matches('/');
    let is_dir = normalized.ends_with('/') || normalized == "/";

    let md_target = if is_dir {
        format!("{}index.md", target)
    } else {
        format!("{}.md", target)
    };

    if let Some(md_path) = secure_join(&state.base_dir, &md_target)
        && let Ok(file) = File::open(&md_path)
        && let Ok(metadata) = file.metadata()
        && metadata.is_file()
    {
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        return serve_markdown(stream, file, &md_path, mtime, keep_alive, state);
    }

    if !is_dir
        && let Some(target_path) = secure_join(&state.base_dir, target)
        && let Ok(file) = File::open(&target_path)
        && let Ok(metadata) = file.metadata()
        && metadata.is_dir()
    {
        let encoded_location =
            utf8_percent_encode(&format!("{}/", normalized), PATH_ENCODE_SET).to_string();
        let escaped_html = escape_html(&normalized);
        let redirect_html = format!(
            "301 Moved Permanently: <a href=\"{}\">{}/</a>",
            encoded_location, escaped_html
        );

        send_headers(
            stream,
            301,
            "text/html",
            redirect_html.len() as u64,
            keep_alive,
            state,
            Some(&format!("Location: {}\r\n", encoded_location)),
        )?;
        stream.write_all(redirect_html.as_bytes())?;
        stream.flush()?;
        return Ok((keep_alive, 301));
    }

    let static_target = if is_dir {
        format!("{}index.html", target)
    } else {
        target.to_string()
    };

    if let Some(static_path) = secure_join(&state.base_dir, &static_target)
        && let Ok(canon) = static_path.canonicalize()
    {
        let base_canon = state
            .base_dir
            .canonicalize()
            .unwrap_or_else(|_| state.base_dir.clone());
        if !canon.starts_with(&base_canon) {
            return send_error(stream, 403, b"403 Forbidden", keep_alive, state);
        }

        if let Some(ext) = canon.extension()
            && ext.to_string_lossy().eq_ignore_ascii_case("md")
        {
            return send_error(stream, 403, b"403 Forbidden", keep_alive, state);
        }

        if let Ok(mut file) = File::open(&canon)
            && let Ok(metadata) = file.metadata()
            && metadata.is_file()
        {
            let mime = get_mime_type(&canon);
            let mut range_start = 0;
            let mut range_end = metadata.len().saturating_sub(1);
            let mut is_partial = false;

            for h in headers {
                if h.name.eq_ignore_ascii_case("range") {
                    if let Ok(range_val) = std::str::from_utf8(h.value)
                        && let Some(stripped) = range_val.strip_prefix("bytes=")
                    {
                        if stripped.contains(',') {
                            break;
                        }

                        let parts: Vec<&str> = stripped.split('-').collect();
                        if parts.len() == 2 {
                            let start_str = parts[0].trim();
                            let end_str = parts[1].trim();

                            if start_str.is_empty() && !end_str.is_empty() {
                                if let Ok(suffix) = end_str.parse::<u64>()
                                    && suffix > 0
                                {
                                    range_start = metadata.len().saturating_sub(suffix);
                                    range_end = metadata.len().saturating_sub(1);
                                    is_partial = true;
                                }
                            } else if !start_str.is_empty()
                                && let Ok(s) = start_str.parse::<u64>()
                            {
                                range_start = s;
                                is_partial = true;
                                if !end_str.is_empty() {
                                    if let Ok(e) = end_str.parse::<u64>() {
                                        range_end = e.min(metadata.len().saturating_sub(1));
                                    }
                                } else {
                                    range_end = metadata.len().saturating_sub(1);
                                }
                            }
                        }
                    }
                    break;
                }
            }

            if is_partial && (range_start > range_end || range_start >= metadata.len()) {
                let extra = format!("Content-Range: bytes */{}\r\n", metadata.len());
                send_headers(
                    stream,
                    416,
                    "text/plain",
                    21,
                    keep_alive,
                    state,
                    Some(&extra),
                )?;
                stream.write_all(b"Range Not Satisfiable")?;
                stream.flush()?;
                return Ok((keep_alive, 416));
            }

            let content_length = if metadata.len() == 0 {
                0
            } else {
                range_end - range_start + 1
            };
            let status = if is_partial { 206 } else { 200 };

            let mut extra_headers = String::with_capacity(128);
            if !mime.contains("html") {
                extra_headers.push_str("Cache-Control: public, max-age=86400\r\n");
            }
            extra_headers.push_str("Accept-Ranges: bytes\r\n");
            if is_partial {
                extra_headers.push_str(&format!(
                    "Content-Range: bytes {}-{}/{}\r\n",
                    range_start,
                    range_end,
                    metadata.len()
                ));
                file.seek(SeekFrom::Start(range_start))?;
            }

            send_headers(
                stream,
                status,
                &mime,
                content_length,
                keep_alive,
                state,
                Some(&extra_headers),
            )?;

            if is_partial {
                let mut reader =
                    std::io::BufReader::with_capacity(65536, file.take(content_length));
                std::io::copy(&mut reader, stream)?;
            } else {
                match stream {
                    LumenStream::Plain(s) => {
                        std::io::copy(&mut file, s)?;
                    }
                    LumenStream::Tls(s) => {
                        std::io::copy(&mut file, s)?;
                    }
                }
            }

            stream.flush()?;
            return Ok((keep_alive, status));
        }
    }

    send_error(
        stream,
        404,
        state.config.paths.fallback_404.as_bytes(),
        keep_alive,
        state,
    )
}

pub fn is_keep_alive(req: &Request) -> bool {
    let is_http11 = req.version.unwrap_or(0) == 1;
    if let Some(h) = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("connection"))
        && let Ok(val) = std::str::from_utf8(h.value)
    {
        if val.eq_ignore_ascii_case("keep-alive") {
            return true;
        }
        if val.eq_ignore_ascii_case("close") {
            return false;
        }
        return val.to_lowercase().contains("keep-alive");
    }
    is_http11
}

pub fn send_headers(
    stream: &mut LumenStream,
    status: u16,
    content_type: &str,
    length: u64,
    keep_alive: bool,
    state: &ServerState,
    extra_headers: Option<&str>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        416 => "Range Not Satisfiable",
        431 => "Header Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };

    let mut buf = Vec::with_capacity(512);
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let date_header = httpdate::fmt_http_date(SystemTime::now());
    let mut num_buf = itoa::Buffer::new();

    write!(
        &mut buf,
        "HTTP/1.1 {} {}\r\nDate: {}\r\nContent-Type: {}\r\nContent-Length: ",
        status, reason, date_header, content_type
    )
    .unwrap();

    buf.extend_from_slice(num_buf.format(length).as_bytes());
    buf.extend_from_slice(b"\r\nConnection: ");
    buf.extend_from_slice(conn.as_bytes());
    buf.extend_from_slice(b"\r\n");

    if let Some(extra) = extra_headers {
        buf.extend_from_slice(extra.as_bytes());
    }
    buf.extend_from_slice(&state.precomputed_headers);
    buf.extend_from_slice(b"\r\n");

    stream.write_all(&buf)
}

pub fn send_response(
    stream: &mut LumenStream,
    status: u16,
    body: &[u8],
    content_type: &str,
    keep_alive: bool,
    state: &ServerState,
    extra_headers: Option<&str>,
) -> std::io::Result<(bool, u16)> {
    send_headers(
        stream,
        status,
        content_type,
        body.len() as u64,
        keep_alive,
        state,
        extra_headers,
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok((keep_alive, status))
}

pub fn send_error(
    stream: &mut LumenStream,
    status: u16,
    message: &[u8],
    keep_alive: bool,
    state: &ServerState,
) -> std::io::Result<(bool, u16)> {
    let content_type = if message.starts_with(b"<") {
        "text/html; charset=utf-8"
    } else {
        "text/plain"
    };
    send_response(
        stream,
        status,
        message,
        content_type,
        keep_alive,
        state,
        None,
    )
}
