use bytes::Bytes;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use std::{fs, sync::Arc, sync::OnceLock, time::SystemTime};

use crate::{
    state::{CacheEntry, ServerState},
    utils::{
        escape_html, get_mime_type, is_compressible, markdown_to_html, secure_join,
        split_frontmatter,
    },
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

#[derive(Clone, Copy, PartialEq)]
pub enum Encoding {
    Brotli,
    Gzip,
    None,
}

pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub accept_encoding: String,
    pub range: Option<String>,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
    pub keep_alive: bool,
}

pub enum ResponseBody {
    Bytes(Bytes),
    Stream(std::fs::File, u64, u64),
}

pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Option<ResponseBody>,
    pub clen: usize,
    pub extra_headers: Vec<(String, String)>,
    pub keep_alive: bool,
}

pub fn determine_encoding(accept_enc: &str) -> Encoding {
    if accept_enc.contains("br") {
        Encoding::Brotli
    } else if accept_enc.contains("gzip") {
        Encoding::Gzip
    } else {
        Encoding::None
    }
}

fn compress_brotli(data: &[u8]) -> Bytes {
    let mut output = Vec::with_capacity(data.len() / 2);
    let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 4, 20);
    let _ = std::io::Write::write_all(&mut writer, data);
    drop(writer);
    Bytes::from(output)
}

fn compress_gzip(data: &[u8]) -> Bytes {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(data.len() / 2),
        flate2::Compression::default(),
    );
    let _ = std::io::Write::write_all(&mut encoder, data);
    Bytes::from(encoder.finish().unwrap_or_default())
}

pub fn build_response(
    keep_alive: bool,
    status: u16,
    content_type: &str,
    body: Option<ResponseBody>,
    clen: usize,
    extra: Vec<(String, String)>,
) -> HttpResponse {
    HttpResponse {
        status,
        content_type: content_type.to_string(),
        body,
        clen,
        extra_headers: extra,
        keep_alive,
    }
}

fn check_conditional(req: &HttpRequest, etag: &str, last_mod: &str) -> bool {
    if let Some(inm) = &req.if_none_match
        && inm == etag
    {
        return true;
    }
    if let Some(ims) = &req.if_modified_since
        && ims == last_mod
    {
        return true;
    }
    false
}

fn extract_encoded_body(
    state: &ServerState,
    cache_key: &std::path::PathBuf,
    entry: &CacheEntry,
    encoding: Encoding,
    use_compression: bool,
    hdrs: &mut Vec<(String, String)>,
) -> Option<Bytes> {
    if use_compression {
        hdrs.push(("Vary".into(), "Accept-Encoding".into()));
        match encoding {
            Encoding::Brotli => {
                let mut added_size = 0;
                let br_bytes = entry
                    .br
                    .get_or_init(|| {
                        let c = compress_brotli(&entry.raw);
                        added_size = c.len();
                        c
                    })
                    .clone();
                if added_size > 0 {
                    state.add_cache_size(cache_key, added_size);
                }
                hdrs.push(("Content-Encoding".into(), "br".into()));
                return Some(br_bytes);
            }
            Encoding::Gzip => {
                let mut added_size = 0;
                let gz_bytes = entry
                    .gz
                    .get_or_init(|| {
                        let c = compress_gzip(&entry.raw);
                        added_size = c.len();
                        c
                    })
                    .clone();
                if added_size > 0 {
                    state.add_cache_size(cache_key, added_size);
                }
                hdrs.push(("Content-Encoding".into(), "gzip".into()));
                return Some(gz_bytes);
            }
            _ => {}
        }
    }
    Some(entry.raw.clone())
}

pub fn serve_markdown(
    state: &ServerState,
    md_path: &std::path::Path,
    mtime: SystemTime,
    encoding: Encoding,
    keep_alive: bool,
    is_head: bool,
    req: &HttpRequest,
) -> HttpResponse {
    let cache_key = md_path.to_path_buf();
    let use_compression = state.config.performance.enable_compression;
    let enc_suffix = match encoding {
        Encoding::Brotli => "-br",
        Encoding::Gzip => "-gz",
        Encoding::None => "",
    };

    if state.config.performance.enable_caching
        && let Some(entry) = state.page_cache.get(&cache_key)
        && entry.mtime == mtime
    {
        let mut hdrs = Vec::new();
        let body = extract_encoded_body(
            state,
            &cache_key,
            &entry,
            encoding,
            use_compression,
            &mut hdrs,
        );

        let mtime_sec = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let etag = format!("W/\"{:x}-{:x}{}\"", mtime_sec, entry.raw.len(), enc_suffix);
        let last_mod = httpdate::fmt_http_date(mtime);

        hdrs.push(("ETag".into(), etag.clone()));
        hdrs.push(("Last-Modified".into(), last_mod.clone()));

        if check_conditional(req, &etag, &last_mod) {
            return build_response(keep_alive, 304, &entry.content_type, None, 0, hdrs);
        }

        let clen = body.as_ref().map(|b| b.len()).unwrap_or(0);
        return build_response(
            keep_alive,
            200,
            &entry.content_type,
            if is_head {
                None
            } else {
                body.map(ResponseBody::Bytes)
            },
            clen,
            hdrs,
        );
    }

    let max_mb = state.config.performance.max_markdown_size_mb as u64;
    let max_bytes = if max_mb == 0 {
        5 * 1024 * 1024
    } else {
        max_mb * 1024 * 1024
    };

    if let Ok(metadata) = std::fs::metadata(md_path)
        && metadata.len() > max_bytes
    {
        let msg = Bytes::from("Payload Too Large");
        return build_response(
            keep_alive,
            413,
            "text/plain",
            if is_head {
                None
            } else {
                Some(ResponseBody::Bytes(msg.clone()))
            },
            msg.len(),
            vec![],
        );
    }

    if let Ok(content) = fs::read_to_string(md_path) {
        let (mut meta, raw_body) = split_frontmatter(&content);
        let fm_use_cache = meta
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
        let use_cache = state.config.performance.enable_caching && fm_use_cache;
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

        let env = Arc::clone(
            &state
                .theme_state
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .1,
        );

        let html_body = markdown_to_html(raw_body);
        meta.insert("content".to_string(), minijinja::Value::from(html_body));

        if let Ok(template) = env.get_template(&template_name)
            && let Ok(rendered) = template.render(minijinja::Value::from(meta))
        {
            let raw_bytes = Bytes::from(rendered.into_bytes());

            let entry = CacheEntry {
                raw: raw_bytes.clone(),
                br: Arc::new(OnceLock::new()),
                gz: Arc::new(OnceLock::new()),
                content_type: content_type.clone(),
                mtime,
            };
            if use_cache {
                state.cache_put(cache_key.clone(), entry.clone());
            }

            let mut hdrs = Vec::new();
            let body = extract_encoded_body(
                state,
                &cache_key,
                &entry,
                encoding,
                use_compression,
                &mut hdrs,
            );

            let mtime_sec = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let etag = format!("W/\"{:x}-{:x}{}\"", mtime_sec, entry.raw.len(), enc_suffix);
            let last_mod = httpdate::fmt_http_date(mtime);

            hdrs.push(("ETag".into(), etag.clone()));
            hdrs.push(("Last-Modified".into(), last_mod.clone()));

            if check_conditional(req, &etag, &last_mod) {
                return build_response(keep_alive, 304, &content_type, None, 0, hdrs);
            }

            let clen = body.as_ref().map(|b| b.len()).unwrap_or(0);
            return build_response(
                keep_alive,
                200,
                &content_type,
                if is_head {
                    None
                } else {
                    body.map(ResponseBody::Bytes)
                },
                clen,
                hdrs,
            );
        }
    }

    let not_found = Bytes::from(state.config.paths.fallback_404.as_bytes().to_vec());
    let clen = not_found.len();
    build_response(
        keep_alive,
        404,
        "text/html",
        if is_head {
            None
        } else {
            Some(ResponseBody::Bytes(not_found))
        },
        clen,
        vec![],
    )
}

pub fn process_http_request(req: HttpRequest, state: Arc<ServerState>) -> HttpResponse {
    let method = req.method.as_str();
    let path = req.path.as_str();
    let is_head = method == "HEAD";
    let keep_alive = if state.is_running.load(std::sync::atomic::Ordering::Relaxed) {
        req.keep_alive
    } else {
        false
    };

    if method != "GET" && method != "HEAD" {
        let msg = Bytes::from("Method Not Allowed");
        return build_response(
            false,
            405,
            "text/plain",
            if is_head {
                None
            } else {
                Some(ResponseBody::Bytes(msg.clone()))
            },
            msg.len(),
            vec![("Allow".into(), "GET, HEAD".into())],
        );
    }

    let decoded_path = percent_decode_str(path)
        .decode_utf8()
        .unwrap_or_else(|_| path.into());
    let normalized = decoded_path
        .split('?')
        .next()
        .unwrap_or("/")
        .replace('\\', "/");

    let has_hidden = normalized
        .split('/')
        .any(|part| part.starts_with('.') && part != ".well-known");
    if normalized.contains("..") || has_hidden {
        let msg = Bytes::from("403 Forbidden");
        return build_response(
            keep_alive,
            403,
            "text/plain",
            if is_head {
                None
            } else {
                Some(ResponseBody::Bytes(msg.clone()))
            },
            msg.len(),
            vec![],
        );
    }

    let target = normalized.trim_start_matches('/');
    let is_dir = normalized.ends_with('/') || normalized == "/";
    let encoding = determine_encoding(&req.accept_encoding);
    let md_target = if is_dir {
        format!("{}index.md", target)
    } else {
        format!("{}.md", target)
    };

    if let Some(md_path) = secure_join(&state.base_dir, &md_target)
        && let Ok(canon) = md_path.canonicalize()
        && canon.starts_with(&state.base_canon)
        && let Ok(metadata) = std::fs::metadata(&canon)
        && metadata.is_file()
    {
        return serve_markdown(
            &state,
            &canon,
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            encoding,
            keep_alive,
            is_head,
            &req,
        );
    }

    if !is_dir
        && let Some(target_path) = secure_join(&state.base_dir, target)
        && let Ok(metadata) = std::fs::metadata(&target_path)
        && metadata.is_dir()
    {
        let encoded_location =
            utf8_percent_encode(&format!("{}/", normalized), PATH_ENCODE_SET).to_string();
        let escaped_html = escape_html(&normalized);
        let redirect_html = Bytes::from(format!(
            "301 Moved Permanently: <a href=\"{}\">{}/</a>",
            encoded_location, escaped_html
        ));
        return build_response(
            keep_alive,
            301,
            "text/html",
            if is_head {
                None
            } else {
                Some(ResponseBody::Bytes(redirect_html.clone()))
            },
            redirect_html.len(),
            vec![("Location".into(), encoded_location)],
        );
    }

    let static_target = if is_dir {
        format!("{}index.html", target)
    } else {
        target.to_string()
    };

    if let Some(static_path) = secure_join(&state.base_dir, &static_target)
        && let Ok(canon) = static_path.canonicalize()
        && canon.starts_with(&state.base_canon)
        && let Ok(metadata) = std::fs::metadata(&canon)
        && metadata.is_file()
    {
        if canon
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
        {
            let msg = Bytes::from("403 Forbidden");
            return build_response(
                keep_alive,
                403,
                "text/plain",
                if is_head {
                    None
                } else {
                    Some(ResponseBody::Bytes(msg.clone()))
                },
                msg.len(),
                vec![],
            );
        }

        let file_len = metadata.len() as usize;
        let mime = get_mime_type(&canon);
        let compressible = state.config.performance.enable_compression && is_compressible(&mime);
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let mtime_sec = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cache_key = canon.clone();

        let enc_suffix = match encoding {
            Encoding::Brotli => "-br",
            Encoding::Gzip => "-gz",
            Encoding::None => "",
        };
        let etag = format!("W/\"{:x}-{:x}{}\"", mtime_sec, file_len, enc_suffix);
        let last_mod = httpdate::fmt_http_date(mtime);

        let mut range_start = 0;
        let mut range_end = file_len.saturating_sub(1);
        let mut is_partial = false;

        if let Some(range_val) = &req.range
            && let Some(stripped) = range_val.strip_prefix("bytes=")
            && !stripped.contains(',')
        {
            let parts: Vec<&str> = stripped.split('-').collect();
            if parts.len() == 2 {
                let (start_str, end_str) = (parts[0].trim(), parts[1].trim());
                if start_str.is_empty() && !end_str.is_empty() {
                    if let Ok(suffix) = end_str.parse::<usize>() {
                        range_start = file_len.saturating_sub(suffix);
                        range_end = file_len.saturating_sub(1);
                        is_partial = true;
                    }
                } else if !start_str.is_empty()
                    && let Ok(s) = start_str.parse::<usize>()
                {
                    range_start = s;
                    range_end = if let Ok(e) = end_str.parse::<usize>() {
                        e.min(file_len.saturating_sub(1))
                    } else {
                        file_len.saturating_sub(1)
                    };
                    is_partial = true;
                }
            }
        }

        if is_partial && (range_start > range_end || range_start >= file_len) {
            let msg = Bytes::from("Range Not Satisfiable");
            return build_response(
                keep_alive,
                416,
                "text/plain",
                if is_head {
                    None
                } else {
                    Some(ResponseBody::Bytes(msg.clone()))
                },
                msg.len(),
                vec![("Content-Range".into(), format!("bytes */{}", file_len))],
            );
        }

        if !is_partial && check_conditional(&req, &etag, &last_mod) {
            return build_response(
                keep_alive,
                304,
                &mime,
                None,
                0,
                vec![("ETag".into(), etag), ("Last-Modified".into(), last_mod)],
            );
        }

        const STREAM_THRESHOLD: usize = 10 * 1024 * 1024; // 10MB bypass memory cache natively

        if state.config.performance.enable_caching
            && let Some(entry) = state.page_cache.get(&cache_key)
            && entry.mtime == mtime
        {
            if is_partial {
                let sliced_body = entry.raw.slice(range_start..range_end + 1);
                return build_response(
                    keep_alive,
                    206,
                    &mime,
                    if is_head {
                        None
                    } else {
                        Some(ResponseBody::Bytes(sliced_body))
                    },
                    range_end - range_start + 1,
                    vec![
                        (
                            "Content-Range".into(),
                            format!("bytes {}-{}/{}", range_start, range_end, file_len),
                        ),
                        ("Accept-Ranges".into(), "bytes".into()),
                    ],
                );
            } else {
                let mut hdrs = Vec::new();
                let body = extract_encoded_body(
                    &state,
                    &cache_key,
                    &entry,
                    encoding,
                    compressible,
                    &mut hdrs,
                );
                hdrs.push(("Accept-Ranges".into(), "bytes".into()));
                hdrs.push(("Cache-Control".into(), "public, max-age=86400".into()));
                hdrs.push(("ETag".into(), etag));
                hdrs.push(("Last-Modified".into(), last_mod));
                let clen = body.as_ref().map(|b| b.len()).unwrap_or(0);
                return build_response(
                    keep_alive,
                    200,
                    &mime,
                    if is_head {
                        None
                    } else {
                        body.map(ResponseBody::Bytes)
                    },
                    clen,
                    hdrs,
                );
            }
        }

        if file_len > STREAM_THRESHOLD
            && let Ok(file) = std::fs::File::open(&canon)
        {
            if is_partial {
                let clen = range_end - range_start + 1;
                return build_response(
                    keep_alive,
                    206,
                    &mime,
                    if is_head {
                        None
                    } else {
                        Some(ResponseBody::Stream(
                            file,
                            range_start as u64,
                            range_end as u64,
                        ))
                    },
                    clen,
                    vec![
                        (
                            "Content-Range".into(),
                            format!("bytes {}-{}/{}", range_start, range_end, file_len),
                        ),
                        ("Accept-Ranges".into(), "bytes".into()),
                    ],
                );
            } else {
                return build_response(
                    keep_alive,
                    200,
                    &mime,
                    if is_head {
                        None
                    } else {
                        Some(ResponseBody::Stream(
                            file,
                            0,
                            file_len.saturating_sub(1) as u64,
                        ))
                    },
                    file_len,
                    vec![
                        ("Accept-Ranges".into(), "bytes".into()),
                        ("ETag".into(), etag),
                        ("Last-Modified".into(), last_mod),
                    ],
                );
            }
        }

        if let Ok(buf) = fs::read(&canon) {
            let raw_bytes = Bytes::from(buf);
            let entry = CacheEntry {
                raw: raw_bytes.clone(),
                br: Arc::new(OnceLock::new()),
                gz: Arc::new(OnceLock::new()),
                content_type: mime.clone(),
                mtime,
            };
            if state.config.performance.enable_caching {
                state.cache_put(cache_key.clone(), entry.clone());
            }

            if is_partial {
                let sliced_body = raw_bytes.slice(range_start..range_end + 1);
                return build_response(
                    keep_alive,
                    206,
                    &mime,
                    if is_head {
                        None
                    } else {
                        Some(ResponseBody::Bytes(sliced_body))
                    },
                    range_end - range_start + 1,
                    vec![
                        (
                            "Content-Range".into(),
                            format!("bytes {}-{}/{}", range_start, range_end, file_len),
                        ),
                        ("Accept-Ranges".into(), "bytes".into()),
                    ],
                );
            } else {
                let mut hdrs = Vec::new();
                let body = extract_encoded_body(
                    &state,
                    &cache_key,
                    &entry,
                    encoding,
                    compressible,
                    &mut hdrs,
                );
                hdrs.push(("Accept-Ranges".into(), "bytes".into()));
                hdrs.push(("Cache-Control".into(), "public, max-age=86400".into()));
                hdrs.push(("ETag".into(), etag));
                hdrs.push(("Last-Modified".into(), last_mod));
                let clen = body.as_ref().map(|b| b.len()).unwrap_or(0);
                return build_response(
                    keep_alive,
                    200,
                    &mime,
                    if is_head {
                        None
                    } else {
                        body.map(ResponseBody::Bytes)
                    },
                    clen,
                    hdrs,
                );
            }
        }
    }

    let not_found = Bytes::from(state.config.paths.fallback_404.as_bytes().to_vec());
    let clen = not_found.len();
    build_response(
        keep_alive,
        404,
        "text/html; charset=utf-8",
        if is_head {
            None
        } else {
            Some(ResponseBody::Bytes(not_found))
        },
        clen,
        vec![],
    )
}
