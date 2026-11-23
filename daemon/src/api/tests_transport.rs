use super::*;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

struct TestServer {
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    workers: Vec<thread::JoinHandle<()>>,
    addr: SocketAddr,
}

impl TestServer {
    fn new(shared: Arc<Shared>, max_connections: usize) -> Self {
        let mut bounds = limits();
        bounds.max_connections = max_connections;
        bounds.connection_timeout = Duration::from_millis(600);
        let server = Arc::new(Server::http_with_limits("127.0.0.1:0", bounds).unwrap());
        let addr = server.server_addr().to_ip().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let workers = (0..HTTP_WORKERS)
            .map(|_| {
                let server = server.clone();
                let shared = shared.clone();
                let stop = stop.clone();
                thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        if let Ok(Some(request)) = server.recv_timeout(Duration::from_millis(30)) {
                            handle(request, &shared);
                        }
                    }
                })
            })
            .collect();
        Self {
            server,
            stop,
            workers,
            addr,
        }
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
    }

    fn request(&self, path: &str, auth: bool) -> String {
        let mut stream = self.connect();
        let credential = if auth {
            "Authorization: Bearer credential\r\n"
        } else {
            ""
        };
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {}\r\n{credential}\r\n",
            self.addr
        )
        .unwrap();
        response(&mut stream)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for _ in &self.workers {
            self.server.unblock();
        }
        for worker in self.workers.drain(..) {
            worker.join().unwrap();
        }
    }
}

fn response(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    match stream.read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(error) => panic!("response did not finish within timeout: {error}"),
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn slow_body_does_not_block_another_client_and_dripping_does_not_extend_deadline() {
    let state = super::tests::shared("credential");
    let server = TestServer::new(state.clone(), 8);
    let mut slow = server.connect();
    write!(slow,"POST /api/action HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer credential\r\nContent-Length: 4096\r\n\r\n",server.addr).unwrap();
    let started = Instant::now();
    let mut writer = slow.try_clone().unwrap();
    let drip = thread::spawn(move || {
        for _ in 0..30 {
            if writer.write_all(b"x").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    let answer = server.request("/api/status", true);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "unrelated request blocked behind slow body"
    );
    let _ = response(&mut slow);
    assert!(
        started.elapsed() < Duration::from_millis(1400),
        "dripping extended absolute deadline"
    );
    assert!(state.with(|data| data.action.is_none()));
    drip.join().unwrap();
}

#[test]
fn connection_slots_are_bounded_and_reclaimed_after_slow_headers() {
    let server = TestServer::new(super::tests::shared("credential"), 2);
    let mut one = server.connect();
    let mut two = server.connect();
    one.write_all(b"GET / HTTP/1.1\r\nHost:").unwrap();
    two.write_all(b"GET / HTTP/1.1\r\nHost:").unwrap();
    let mut excess = server.connect();
    assert!(
        response(&mut excess).is_empty(),
        "excess connection must be dropped"
    );
    let _ = response(&mut one);
    let _ = response(&mut two);
    assert!(server
        .request("/api/status", true)
        .starts_with("HTTP/1.1 200"));
}

#[test]
fn oversized_header_and_chunked_body_are_bounded() {
    let state = super::tests::shared("credential");
    let server = TestServer::new(state.clone(), 8);
    let mut header = server.connect();
    let oversized = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nX-Long: {}",
        server.addr,
        "a".repeat(limits().max_header_bytes)
    );
    let _ = header.write_all(oversized.as_bytes());
    let answer = response(&mut header);
    assert!(!answer.starts_with("HTTP/1.1 200"));

    let mut body = server.connect();
    let bytes = "x".repeat(MAX_BODY + 1);
    let request = format!("POST /api/action HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer credential\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{bytes}\r\n0\r\n\r\n",server.addr,bytes.len());
    let _ = body.write_all(request.as_bytes());
    let answer = response(&mut body);
    assert!(answer.starts_with("HTTP/1.1 413"), "{answer}");
    assert!(state.with(|data| data.action.is_none()));
}

#[test]
fn incomplete_content_length_is_rejected_without_executing_action() {
    let state = super::tests::shared("credential");
    let server = TestServer::new(state.clone(), 8);
    let mut stream = server.connect();
    write!(stream,"POST /api/action HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer credential\r\nContent-Length: 4096\r\n\r\n{{\"action\":\"predict\"}}",server.addr).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let answer = response(&mut stream);
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
    assert!(state.with(|data| data.action.is_none()));
}

#[test]
fn limited_connection_processes_only_one_request_and_closes() {
    let server = TestServer::new(super::tests::shared("credential"), 8);
    let mut stream = server.connect();
    let request = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", server.addr);
    stream
        .write_all(format!("{request}{request}").as_bytes())
        .unwrap();
    let answer = response(&mut stream);
    assert_eq!(answer.matches("HTTP/1.1 200").count(), 1);
    assert!(answer.to_ascii_lowercase().contains("connection: close"));
}

#[test]
fn diagnostics_require_authentication() {
    let server = TestServer::new(super::tests::shared("credential"), 8);
    assert!(server
        .request("/api/diagnostics", false)
        .starts_with("HTTP/1.1 401"));
    let answer = server.request("/api/diagnostics", true);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(!answer.contains("credential"));
}

#[test]
fn listener_rejects_non_loopback_configuration_before_binding() {
    let error = serve(super::tests::shared("credential"), "0.0.0.0", 0).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn localhost_bind_is_normalized_without_dns_and_ipv6_is_preserved() {
    assert_eq!(
        bind_address("localhost", 8787).unwrap(),
        "127.0.0.1:8787".parse().unwrap()
    );
    assert_eq!(
        bind_address("::1", 8787).unwrap(),
        "[::1]:8787".parse().unwrap()
    );
    assert!(bind_address("localhost.example", 8787).is_err());
}

#[test]
fn chunked_json_works_and_ambiguous_or_large_declared_bodies_are_rejected() {
    let state = super::tests::shared("credential");
    let server = TestServer::new(state.clone(), 8);
    for (headers, body, expected) in [
        (
            "Transfer-Encoding: chunked\r\n",
            "18\r\n{\"action\":\"reset_model\"}\r\n0\r\n\r\n",
            200,
        ),
        (
            "Transfer-Encoding: chunked\r\nContent-Length: 0\r\n",
            "0\r\n\r\n",
            400,
        ),
        ("Content-Length: 65537\r\n", "", 413),
        ("Content-Length: 0\r\nContent-Length: 0\r\n", "", 400),
    ] {
        let mut stream = server.connect();
        write!(stream,"POST /api/action HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer credential\r\n{headers}\r\n{body}",server.addr).unwrap();
        let answer = response(&mut stream);
        assert!(
            answer.starts_with(&format!("HTTP/1.1 {expected}")),
            "{answer}"
        );
    }
    assert!(state.with(|data| data.action == Some(Action::ResetModel)));
}

#[test]
fn client_that_does_not_read_response_releases_its_connection_slot() {
    let state = super::tests::shared("credential");
    std::fs::create_dir_all(state.paths.telemetry()).unwrap();
    std::fs::write(
        state.paths.telemetry().join("history.csv"),
        vec![b'x'; 8 * 1024 * 1024],
    )
    .unwrap();
    let server = TestServer::new(state, 1);
    let mut stalled = server.connect();
    write!(
        stalled,
        "GET /api/export.csv HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer credential\r\n\r\n",
        server.addr
    )
    .unwrap();
    // Never read the large reply: the socket write must still meet its deadline.
    thread::sleep(Duration::from_millis(850));
    assert!(server
        .request("/api/status", true)
        .starts_with("HTTP/1.1 200"));
}

#[test]
fn concurrent_config_updates_preserve_each_committed_patch_before_runtime_reload() {
    let state = super::tests::shared("credential");
    let server = TestServer::new(state.clone(), 8);
    let mut clients = Vec::new();
    for body in [
        r#"{"policy":{"thermal":{"enter_light_c":68.5}}}"#,
        r#"{"policy":{"memory":{"psi_some_enter":15.0}}}"#,
    ] {
        let addr = server.addr;
        clients.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            write!(stream,"POST /api/config HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer credential\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
            assert!(response(&mut stream).starts_with("HTTP/1.1 200"));
        }));
    }
    for client in clients {
        client.join().unwrap();
    }
    let saved = crate::config::Config::load(&state.paths.config()).unwrap();
    assert_eq!(saved.policy.thermal.enter_light_c, 68.5);
    assert_eq!(saved.policy.memory.psi_some_enter, 15.0);
}
