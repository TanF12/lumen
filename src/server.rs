use httparse::Request;
use minijinja::Environment;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::Config,
    http::{is_keep_alive, send_error, serve_path},
    state::{ServerState, ShardedLruCache},
    thread_pool::ThreadPool,
};

pub fn start_server(config: Config) {
    let base_dir = std::env::current_dir()
        .unwrap()
        .join(&config.paths.content_dir);

    let theme_mtime = fs::metadata(&config.paths.theme_file)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut env = Environment::new();
    let theme_html = fs::read_to_string(&config.paths.theme_file)
        .unwrap_or_else(|_| "{{ content|safe }}".to_string());
    env.add_template_owned("index", theme_html).unwrap();

    let precomputed_headers = format!(
        "Server: {}\r\nX-Content-Type-Options: {}\r\nX-Frame-Options: {}\r\nContent-Security-Policy: {}\r\nAccess-Control-Allow-Origin: {}\r\n",
        config.server.name,
        config.security.x_content_type_options,
        config.security.x_frame_options,
        config.security.content_security_policy,
        config.security.cors_allow_origin
    );

    let state = Arc::new(ServerState {
        base_dir,
        page_cache: ShardedLruCache::new(config.performance.max_cache_items),
        theme_state: RwLock::new((theme_mtime, Arc::new(env))),
        config: config.clone(),
        precomputed_headers,
        active_connections: AtomicUsize::new(0),
    });

    let host_port = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&host_port).expect("Failed to bind to port");
    info!("Server started at http://{}", host_port);

    let pool = ThreadPool::new(config.server.threads, config.server.queue_size);

    for mut stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);

        let state_clone = Arc::clone(&state);

        match stream.try_clone() {
            Ok(stream_clone) => {
                if pool
                    .execute(move || handle_connection(stream_clone, state_clone))
                    .is_err()
                {
                    warn!("Queue full, shedding load with 503.");
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                    let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                }
            }
            Err(e) => {
                error!("Failed to clone stream: {}", e);
            }
        }
    }
}

struct ConnectionGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> Drop for ConnectionGuard<'a> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<ServerState>) {
    state.active_connections.fetch_add(1, Ordering::SeqCst);
    let _guard = ConnectionGuard {
        counter: &state.active_connections,
    };

    let default_timeout = Duration::from_secs(state.config.server.read_timeout_secs);
    let idle_ka_timeout = Duration::from_secs(2);

    let _ = stream.set_write_timeout(Some(Duration::from_secs(
        state.config.server.write_timeout_secs,
    )));

    let mut buffer = vec![0; state.config.performance.connection_buffer_size];
    let mut read_offset = 0;
    let mut is_first_request = true;
    let mut absolute_deadline;

    loop {
        if !is_first_request {
            absolute_deadline = Instant::now() + idle_ka_timeout;
            let _ = stream.set_read_timeout(Some(idle_ka_timeout));
        } else {
            absolute_deadline = Instant::now() + default_timeout;
            let _ = stream.set_read_timeout(Some(default_timeout));
        }

        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers);

        match req.parse(&buffer[..read_offset]) {
            Ok(httparse::Status::Complete(header_len)) => {
                let mut keep_alive = is_keep_alive(&req);

                if keep_alive
                    && state.active_connections.load(Ordering::Relaxed)
                        >= state.config.server.threads
                {
                    keep_alive = false;
                }

                let has_body = req.headers.iter().any(|h| {
                    if h.name.eq_ignore_ascii_case("content-length") {
                        let val = std::str::from_utf8(h.value).unwrap_or("").trim();
                        val != "0" && !val.is_empty()
                    } else {
                        h.name.eq_ignore_ascii_case("transfer-encoding")
                    }
                });

                let method = req.method.unwrap_or("GET");
                let path = req.path.unwrap_or("/");

                let (keep_alive_result, status) = if method != "GET" || has_body {
                    send_error(&mut stream, 405, b"Method Not Allowed", false, &state)
                        .unwrap_or((false, 500))
                } else {
                    serve_path(&mut stream, path, req.headers, &state, keep_alive)
                        .unwrap_or((false, 500))
                };

                info!("{} {} {}", method, path, status);

                buffer.copy_within(header_len..read_offset, 0);
                read_offset -= header_len;
                is_first_request = false;

                if !keep_alive_result {
                    break;
                }
                continue;
            }
            Ok(httparse::Status::Partial) => {
                if read_offset == buffer.len() {
                    let _ = send_error(
                        &mut stream,
                        431,
                        b"Request Header Fields Too Large",
                        false,
                        &state,
                    );
                    break;
                }
            }
            Err(_) => {
                let _ = send_error(&mut stream, 400, b"Bad Request", false, &state);
                break;
            }
        }

        let now = Instant::now();
        if now >= absolute_deadline {
            if is_first_request {
                let _ = send_error(&mut stream, 408, b"Request Timeout", false, &state);
            }
            break;
        }

        let _ = stream.set_read_timeout(Some(absolute_deadline.duration_since(now)));

        match stream.read(&mut buffer[read_offset..]) {
            Ok(0) => break, // EOF
            Ok(n) => read_offset += n,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if is_first_request {
                    let _ = send_error(&mut stream, 408, b"Request Timeout", false, &state);
                }
                break;
            }
            Err(e) => {
                debug!("Connection read error: {}", e);
                break;
            }
        }
    }
}
