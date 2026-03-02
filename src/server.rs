use bytes::Bytes;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token, Waker};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    net::IpAddr,
    sync::{Arc, RwLock, atomic::AtomicBool, mpsc},
    thread,
    time::{Duration, Instant, SystemTime},
};
use tracing::{error, info};

use crate::{
    config::Config,
    http::{HttpRequest, HttpResponse, ResponseBody, process_http_request},
    state::{ServerState, ShardedLruCache},
    thread_pool::ThreadPool,
};

const SERVER_TOKEN: Token = Token(usize::MAX);
const WAKER_TOKEN: Token = Token(usize::MAX - 1);
const MAX_CONNECTIONS: usize = 10_000;

pub enum MainMessage {
    HttpResponse(usize, HttpResponse),
    FileChunk(usize, std::fs::File, u64, u64, Bytes),
}

pub enum WriteChunk {
    Raw(Bytes),
    Stream(std::fs::File, u64, u64),
}

struct Connection {
    stream: TcpStream,
    ip: IpAddr,
    read_buf: Vec<u8>,
    write_queue: VecDeque<WriteChunk>,
    keep_alive: bool,
    state: ConnState,
    created_at: Instant,
    last_active: Instant,
}

#[derive(PartialEq)]
enum ConnState {
    Idle,
    Writing,
}

pub fn start_server(config: Config) {
    let base_dir = std::env::current_dir()
        .unwrap_or_default()
        .join(&config.paths.content_dir);
    let base_canon = base_dir.canonicalize().unwrap_or_else(|_| base_dir.clone());

    let mut precomp = format!(
        "Server: {}\r\nX-Content-Type-Options: {}\r\nX-Frame-Options: {}\r\nContent-Security-Policy: {}\r\n",
        config.server.name,
        config.security.x_content_type_options,
        config.security.x_frame_options,
        config.security.content_security_policy
    );
    if !config.security.cors_allow_origin.is_empty() {
        precomp.push_str(&format!(
            "Access-Control-Allow-Origin: {}\r\n",
            config.security.cors_allow_origin
        ));
    }
    let precomputed_headers: Arc<[u8]> = precomp.into_bytes().into();

    let cache_mem_bytes = config.performance.max_cache_memory_mb * 1024 * 1024;
    let state = Arc::new(ServerState {
        base_dir,
        base_canon,
        page_cache: ShardedLruCache::new(cache_mem_bytes, usize::MAX),
        dir_cache: ShardedLruCache::new(usize::MAX, 10_000),
        theme_state: RwLock::new((0, Arc::new(minijinja::Environment::new()))),
        config: config.clone(),
        precomputed_headers,
        is_running: Arc::new(AtomicBool::new(true)),
    });

    start_theme_watcher(Arc::clone(&state));

    let host_port = format!("{}:{}", config.server.host, config.server.port);
    let address = match host_port.parse() {
        Ok(addr) => addr,
        Err(e) => {
            error!("Invalid bind address {}: {}", host_port, e);
            std::process::exit(1);
        }
    };

    let mut listener = match TcpListener::bind(address) {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind to {}: {}", host_port, e);
            std::process::exit(1);
        }
    };

    let mut poll = Poll::new().expect("Failed to create Poll instance");
    poll.registry()
        .register(&mut listener, SERVER_TOKEN, Interest::READABLE)
        .expect("Failed to register server listener");

    let waker = Arc::new(Waker::new(poll.registry(), WAKER_TOKEN).expect("Failed to create waker"));
    let pool = ThreadPool::new(config.server.threads, config.server.queue_size);
    let (tx_main, rx_main) = mpsc::channel::<MainMessage>();

    let is_running_clone = Arc::clone(&state.is_running);
    let waker_clone = Arc::clone(&waker);
    ctrlc::set_handler(move || {
        info!("Received Ctrl-C, shutting down gracefully...");
        is_running_clone.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = waker_clone.wake();
    })
    .expect("Error setting Ctrl-C handler");

    let mut connections: HashMap<usize, Connection> = HashMap::new();
    let mut ip_counts: HashMap<IpAddr, usize> = HashMap::new();
    let mut next_token: usize = 0;
    let mut events = Events::with_capacity(2048);
    let mut last_sweep = Instant::now();

    let idle_timeout = Duration::from_secs(config.server.timeout_secs);
    let max_connection_life = Duration::from_secs(120);

    info!("Lumen HTTP server running bound to {}", host_port);

    loop {
        if !state.is_running.load(std::sync::atomic::Ordering::SeqCst) && connections.is_empty() {
            break;
        }

        if let Err(e) = poll.poll(&mut events, Some(Duration::from_millis(500))) {
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }

        let now = Instant::now();
        if now.duration_since(last_sweep) > Duration::from_secs(5) {
            last_sweep = now;
            let mut timed_out = Vec::new();
            for (&token, conn) in &connections {
                if now.duration_since(conn.last_active) > idle_timeout
                    || now.duration_since(conn.created_at) > max_connection_life
                {
                    timed_out.push(token);
                }
            }
            for token in timed_out {
                if let Some(mut c) = connections.remove(&token) {
                    cleanup_connection(&mut c, &poll, &mut ip_counts);
                }
            }
        }

        for event in events.iter() {
            match event.token() {
                SERVER_TOKEN => {
                    if !state.is_running.load(std::sync::atomic::Ordering::SeqCst) {
                        continue;
                    }
                    loop {
                        if connections.len() >= MAX_CONNECTIONS {
                            break;
                        }
                        match listener.accept() {
                            Ok((mut stream, peer)) => {
                                let ip = peer.ip();
                                let count = ip_counts.entry(ip).or_insert(0);
                                if *count > 200 {
                                    continue;
                                }
                                *count += 1;

                                let _ = stream.set_nodelay(true);
                                while connections.contains_key(&next_token) {
                                    next_token = next_token.wrapping_add(1);
                                }
                                let token_id = next_token;
                                next_token = next_token.wrapping_add(1);

                                if poll
                                    .registry()
                                    .register(
                                        &mut stream,
                                        Token(token_id),
                                        Interest::READABLE | Interest::WRITABLE,
                                    )
                                    .is_ok()
                                {
                                    let now = Instant::now();
                                    connections.insert(
                                        token_id,
                                        Connection {
                                            stream,
                                            ip,
                                            read_buf: Vec::with_capacity(4096),
                                            write_queue: VecDeque::with_capacity(16),
                                            keep_alive: true,
                                            state: ConnState::Idle,
                                            created_at: now,
                                            last_active: now,
                                        },
                                    );
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                WAKER_TOKEN => {
                    while let Ok(msg) = rx_main.try_recv() {
                        match msg {
                            MainMessage::HttpResponse(token_id, res) => {
                                let is_done = if let Some(conn) = connections.get_mut(&token_id) {
                                    format_response(conn, &res, &state);
                                    conn.keep_alive = res.keep_alive;
                                    pump_connection(
                                        conn, token_id, true, &pool, &tx_main, &waker, &state,
                                    )
                                } else {
                                    false
                                };
                                if is_done && let Some(mut c) = connections.remove(&token_id) {
                                    cleanup_connection(&mut c, &poll, &mut ip_counts);
                                }
                            }
                            MainMessage::FileChunk(token_id, file, new_offset, end, bytes) => {
                                let is_done = if let Some(conn) = connections.get_mut(&token_id) {
                                    conn.last_active = Instant::now();
                                    if !bytes.is_empty() {
                                        if new_offset <= end {
                                            conn.write_queue.push_front(WriteChunk::Stream(
                                                file, new_offset, end,
                                            ));
                                        }
                                        conn.write_queue.push_front(WriteChunk::Raw(bytes));
                                    }
                                    pump_connection(
                                        conn, token_id, true, &pool, &tx_main, &waker, &state,
                                    )
                                } else {
                                    false
                                };
                                if is_done && let Some(mut c) = connections.remove(&token_id) {
                                    cleanup_connection(&mut c, &poll, &mut ip_counts);
                                }
                            }
                        }
                    }
                }
                Token(token_id) => {
                    let mut done = false;
                    if let Some(conn) = connections.get_mut(&token_id) {
                        conn.last_active = Instant::now();

                        if event.is_readable() {
                            done = handle_read(conn);
                        }

                        if !done {
                            done = pump_connection(
                                conn,
                                token_id,
                                event.is_writable(),
                                &pool,
                                &tx_main,
                                &waker,
                                &state,
                            );
                        }
                    }
                    if done && let Some(mut c) = connections.remove(&token_id) {
                        cleanup_connection(&mut c, &poll, &mut ip_counts);
                    }
                }
            }
        }
    }
}

#[inline(always)]
fn cleanup_connection(conn: &mut Connection, poll: &Poll, ip_counts: &mut HashMap<IpAddr, usize>) {
    let _ = poll.registry().deregister(&mut conn.stream);
    if let Some(count) = ip_counts.get_mut(&conn.ip) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            ip_counts.remove(&conn.ip);
        }
    }
}

#[inline(always)]
fn pump_connection(
    conn: &mut Connection,
    token_id: usize,
    is_writable: bool,
    pool: &ThreadPool,
    tx_main: &mpsc::Sender<MainMessage>,
    waker: &Arc<Waker>,
    state: &Arc<ServerState>,
) -> bool {
    let mut done = false;
    loop {
        if !conn.write_queue.is_empty() && is_writable {
            done = handle_write(conn, token_id, pool, tx_main, waker);
            if done {
                break;
            }
        }

        let mut parsed_something = false;
        if !conn.read_buf.is_empty() && conn.state != ConnState::Writing {
            let (d, p) = try_parse_h1(conn, token_id, pool, tx_main, waker, state);
            done = d;
            parsed_something = p;
        }

        if done || !parsed_something {
            break;
        }
    }
    done
}

fn handle_read(conn: &mut Connection) -> bool {
    let mut buf = [0u8; 8192];
    loop {
        match conn.stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                conn.read_buf.extend_from_slice(&buf[..n]);
                if conn.read_buf.len() > 64 * 1024 {
                    return true;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
            Err(_) => return true,
        }
    }
}

fn handle_write(
    conn: &mut Connection,
    token_id: usize,
    pool: &ThreadPool,
    tx: &mpsc::Sender<MainMessage>,
    waker: &Arc<Waker>,
) -> bool {
    while let Some(chunk) = conn.write_queue.pop_front() {
        match chunk {
            WriteChunk::Raw(mut bytes) => match conn.stream.write(&bytes) {
                Ok(0) => {
                    conn.write_queue.push_front(WriteChunk::Raw(bytes));
                    return true;
                }
                Ok(n) => {
                    if n < bytes.len() {
                        bytes = bytes.slice(n..);
                        conn.write_queue.push_front(WriteChunk::Raw(bytes));
                        return false;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    conn.write_queue.push_front(WriteChunk::Raw(bytes));
                    return false;
                }
                Err(_) => return true,
            },
            WriteChunk::Stream(mut file, offset, end) => {
                let to_read = std::cmp::min(32768, end - offset + 1) as usize;
                if to_read == 0 {
                    continue;
                }
                let tx_cl = tx.clone();
                let w_cl = Arc::clone(waker);
                if pool
                    .execute(move || {
                        let mut buf = vec![0u8; to_read];
                        let _ = file.seek(SeekFrom::Start(offset));
                        if let Ok(n) = file.read(&mut buf) {
                            buf.truncate(n);
                            let _ = tx_cl.send(MainMessage::FileChunk(
                                token_id,
                                file,
                                offset + n as u64,
                                end,
                                Bytes::from(buf),
                            ));
                        } else {
                            let _ = tx_cl.send(MainMessage::FileChunk(
                                token_id,
                                file,
                                offset,
                                end,
                                Bytes::new(),
                            ));
                        }
                        let _ = w_cl.wake();
                    })
                    .is_err()
                {
                    return true;
                }
                return false;
            }
        }
    }

    if conn.write_queue.is_empty() {
        if conn.keep_alive {
            conn.state = ConnState::Idle;
        } else {
            return true;
        }
    }
    false
}

fn format_response(conn: &mut Connection, res: &HttpResponse, state: &ServerState) {
    let mut head = Vec::with_capacity(1024);
    let reason = match res.status {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Error",
    };

    let _ = write!(
        &mut head,
        "HTTP/1.1 {} {}\r\nDate: {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        res.status,
        reason,
        httpdate::fmt_http_date(SystemTime::now()),
        res.content_type,
        res.clen,
        if res.keep_alive {
            "keep-alive"
        } else {
            "close"
        }
    );

    for (k, v) in &res.extra_headers {
        head.extend_from_slice(k.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(v.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(&state.precomputed_headers);
    head.extend_from_slice(b"\r\n");

    conn.write_queue
        .push_back(WriteChunk::Raw(Bytes::from(head)));
    match &res.body {
        Some(ResponseBody::Bytes(b)) => conn.write_queue.push_back(WriteChunk::Raw(b.clone())),
        Some(ResponseBody::Stream(file, start, end)) => {
            if let Ok(dup) = file.try_clone() {
                conn.write_queue
                    .push_back(WriteChunk::Stream(dup, *start, *end));
            }
        }
        None => {}
    }
}

fn try_parse_h1(
    conn: &mut Connection,
    token_id: usize,
    pool: &ThreadPool,
    tx_main: &mpsc::Sender<MainMessage>,
    waker: &Arc<Waker>,
    state: &Arc<ServerState>,
) -> (bool, bool) {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&conn.read_buf) {
        Ok(httparse::Status::Complete(header_len)) => {
            let mut req_struct = HttpRequest {
                method: req.method.unwrap_or("GET").into(),
                path: req.path.unwrap_or("/").into(),
                accept_encoding: "".into(),
                range: None,
                if_none_match: None,
                if_modified_since: None,
                keep_alive: req.version.unwrap_or(0) == 1,
            };

            let (mut clen, mut cl_count, mut has_te) = (0, 0, false);
            for h in req.headers.iter() {
                if h.name.eq_ignore_ascii_case("content-length") {
                    cl_count += 1;
                    if let Ok(s) = std::str::from_utf8(h.value) {
                        clen = s.trim().parse().unwrap_or(0);
                    }
                } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
                    has_te = true;
                } else if h.name.eq_ignore_ascii_case("accept-encoding") {
                    if let Ok(s) = std::str::from_utf8(h.value) {
                        req_struct.accept_encoding = s.into();
                    }
                } else if h.name.eq_ignore_ascii_case("range") {
                    if let Ok(s) = std::str::from_utf8(h.value) {
                        req_struct.range = Some(s.into());
                    }
                } else if h.name.eq_ignore_ascii_case("if-none-match") {
                    if let Ok(s) = std::str::from_utf8(h.value) {
                        req_struct.if_none_match = Some(s.into());
                    }
                } else if h.name.eq_ignore_ascii_case("if-modified-since") {
                    if let Ok(s) = std::str::from_utf8(h.value) {
                        req_struct.if_modified_since = Some(s.into());
                    }
                } else if h.name.eq_ignore_ascii_case("connection")
                    && let Ok(val) = std::str::from_utf8(h.value)
                {
                    if val.eq_ignore_ascii_case("keep-alive") {
                        req_struct.keep_alive = true;
                    } else if val.eq_ignore_ascii_case("close") {
                        req_struct.keep_alive = false;
                    }
                }
            }
            if cl_count > 1 || (cl_count > 0 && has_te) {
                return (true, false);
            }

            let total_len = header_len.saturating_add(clen);
            if conn.read_buf.len() >= total_len {
                conn.read_buf.drain(..total_len);
                conn.state = ConnState::Writing;

                let st = Arc::clone(state);
                let tx = tx_main.clone();
                let w = waker.clone();
                if pool
                    .execute(move || {
                        let res = process_http_request(req_struct, st);
                        let _ = tx.send(MainMessage::HttpResponse(token_id, res));
                        let _ = w.wake();
                    })
                    .is_err()
                {
                    return (true, true);
                }
                (false, true)
            } else {
                (false, false)
            }
        }
        Ok(httparse::Status::Partial) => {
            if conn.read_buf.len() > 65536 {
                return (true, false);
            }
            (false, false)
        }
        Err(_) => (true, false),
    }
}

fn start_theme_watcher(state: Arc<ServerState>) {
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(2));
            let mut max_mtime = SystemTime::UNIX_EPOCH;
            let mut file_count = 0usize;

            let theme_files =
                crate::utils::get_all_files(std::path::Path::new(&state.config.paths.theme_dir), 0);
            for path in &theme_files {
                if let Ok(meta) = fs::metadata(path) {
                    file_count += 1;
                    if let Ok(mt) = meta.modified()
                        && mt > max_mtime
                    {
                        max_mtime = mt;
                    }
                }
            }

            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            use std::hash::Hasher;
            hasher.write(
                &(max_mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs())
                .to_ne_bytes(),
            );
            hasher.write(&file_count.to_ne_bytes());
            let current_hash = hasher.finish();

            let current_cache_hash = state
                .theme_state
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .0;

            if current_cache_hash != current_hash {
                let mut env = minijinja::Environment::new();
                let env_state = Arc::clone(&state);

                env.add_function("list_dir", move |dir_path: String| -> minijinja::Value {
                    let target_dir = crate::utils::secure_join(&env_state.base_dir, &dir_path)
                        .unwrap_or_default();
                    if target_dir.as_os_str().is_empty() {
                        return minijinja::Value::from(Vec::<minijinja::Value>::new());
                    }

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
                                dir_hash =
                                    (dir_hash.rotate_left(3) ^ mtime).wrapping_add(meta.len());
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
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let content = fs::read_to_string(entry.path()).unwrap_or_default();
                        let (mut meta, _) = crate::utils::split_frontmatter(&content);
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
                    env_state.dir_cache_put(target_dir, dir_hash, val.clone());
                    val
                });

                let theme_dir_path = std::path::Path::new(&state.config.paths.theme_dir);
                for path in theme_files {
                    let rel_path = path.strip_prefix(theme_dir_path).unwrap_or(&path);
                    let name = rel_path.to_string_lossy().replace('\\', "/");
                    let content = fs::read_to_string(&path).unwrap_or_default();
                    let _ = env.add_template_owned(name.clone(), content.clone());
                    if name == "index.html" {
                        let _ = env.add_template_owned("index", content);
                    }
                }

                let mut cache = state.theme_state.write().unwrap_or_else(|e| e.into_inner());
                *cache = (current_hash, Arc::new(env));
                state.page_cache.clear();
                state.dir_cache.clear();
            }
        }
    });
}
