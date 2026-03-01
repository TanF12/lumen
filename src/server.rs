use httparse::Request;
use minijinja::Environment;
use rustls::{ServerConnection, StreamOwned};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::{
    fs,
    io::{BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::Config,
    http::{is_keep_alive, send_error, serve_path},
    state::{ServerState, ShardedLruCache},
    thread_pool::ThreadPool,
};

pub enum LumenStream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ServerConnection, TcpStream>>),
}

impl Read for LumenStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for LumenStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

impl LumenStream {
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.set_read_timeout(dur),
            Self::Tls(s) => s.sock.set_read_timeout(dur),
        }
    }
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.set_write_timeout(dur),
            Self::Tls(s) => s.sock.set_write_timeout(dur),
        }
    }
}

fn load_certs(path: &Path) -> std::io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()
}

fn load_private_key(path: &Path) -> std::io::Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    match rustls_pemfile::private_key(&mut reader) {
        Ok(Some(key)) => Ok(key),
        Ok(None) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No private key found",
        )),
        Err(e) => Err(e),
    }
}

pub fn start_server(config: Config) {
    let base_dir = std::env::current_dir()
        .unwrap()
        .join(&config.paths.content_dir);

    let precomputed_headers: Arc<[u8]> = format!(
        "Server: {}\r\nX-Content-Type-Options: {}\r\nX-Frame-Options: {}\r\nContent-Security-Policy: {}\r\nAccess-Control-Allow-Origin: {}\r\n",
        config.server.name,
        config.security.x_content_type_options,
        config.security.x_frame_options,
        config.security.content_security_policy,
        config.security.cors_allow_origin
    ).into_bytes().into();

    let tls_config = if config.tls.enabled {
        info!("Loading TLS certificates...");
        let certs = load_certs(Path::new(&config.tls.cert_path)).expect("Failed to load TLS certs");
        let key = load_private_key(Path::new(&config.tls.key_path))
            .expect("Failed to load TLS private key");

        let mut cfg = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("Bad TLS configuration");

        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Some(Arc::new(cfg))
    } else {
        None
    };

    let running = Arc::new(AtomicBool::new(true));

    let state = Arc::new(ServerState {
        base_dir,
        page_cache: ShardedLruCache::new(config.performance.max_cache_items),
        dir_cache: ShardedLruCache::new(std::cmp::max(1, config.performance.max_cache_items / 4)),
        theme_state: RwLock::new((0, Arc::new(Environment::new()))),
        config: config.clone(),
        precomputed_headers,
        active_connections: AtomicUsize::new(0),
        tls_config,
        is_running: Arc::clone(&running),
    });

    let host_port = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&host_port).expect("Failed to bind to port");
    info!(
        "Server running at {}://{}",
        if config.tls.enabled { "https" } else { "http" },
        host_port
    );

    let r = Arc::clone(&running);
    let host_clone = config.server.host.clone();
    let port_clone = config.server.port;

    ctrlc::set_handler(move || {
        info!("Received shutdown signal. Initiating graceful drain...");
        r.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(format!("{}:{}", host_clone, port_clone));
    })
    .unwrap_or_else(|e| warn!("Error setting Ctrl-C handler: {}", e));

    let pool = ThreadPool::new(config.server.threads, config.server.queue_size);

    for stream_res in listener.incoming() {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        match stream_res {
            Ok(mut stream) => {
                let _ = stream.set_nodelay(true);
                let state_clone = Arc::clone(&state);

                match stream.try_clone() {
                    Ok(stream_clone) => {
                        let lumen_stream = if let Some(tls_cfg) = &state.tls_config {
                            let conn = ServerConnection::new(Arc::clone(tls_cfg)).unwrap();
                            LumenStream::Tls(Box::new(StreamOwned::new(conn, stream_clone)))
                        } else {
                            LumenStream::Plain(stream_clone)
                        };

                        if pool
                            .execute(move || handle_connection(lumen_stream, state_clone))
                            .is_err()
                        {
                            warn!("Queue full, shedding load with 503.");
                            let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                        }
                    }
                    Err(e) => error!("Failed to clone stream: {}", e),
                }
            }
            Err(e) => error!("Failed to accept connection: {}", e),
        }
    }

    info!("Stopped accepting new connections. Waiting for active connections to finish...");
    while state.active_connections.load(Ordering::SeqCst) > 0 {
        std::thread::sleep(Duration::from_millis(50));
    }
    info!("All connections closed. Server gracefully stopped.");
}

struct ConnectionGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> Drop for ConnectionGuard<'a> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_connection(mut stream: LumenStream, state: Arc<ServerState>) {
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

    let mut absolute_deadline = Instant::now() + default_timeout;

    loop {
        let now = Instant::now();
        if now >= absolute_deadline {
            if is_first_request {
                let _ = send_error(&mut stream, 408, b"Request Timeout", false, &state);
            }
            break;
        }

        let _ = stream.set_read_timeout(Some(absolute_deadline.duration_since(now)));

        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers);

        match req.parse(&buffer[..read_offset]) {
            Ok(httparse::Status::Complete(header_len)) => {
                let mut keep_alive = is_keep_alive(&req);

                if keep_alive
                    && (!state.is_running.load(Ordering::Relaxed)
                        || state.active_connections.load(Ordering::Relaxed)
                            >= state.config.server.threads)
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

                absolute_deadline = Instant::now() + idle_ka_timeout;
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
                debug!("Connection read/TLS handshake error: {}", e);
                break;
            }
        }
    }
}
