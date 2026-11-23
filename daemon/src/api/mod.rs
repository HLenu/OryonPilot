//! Local HTTP API + WebUI host.
//!
//! The HTTP layer is `tiny_http` (Apache-2.0/MIT) rather than a hand-written parser:
//! request splitting, chunked bodies, header folding and keep-alive are exactly the
//! places where a home-grown server quietly becomes a security bug.
//!
//! Constraints kept from the first iteration:
//!  * loopback only, enforced twice (bind address + peer check)
//!  * every `/api/*` call needs the token from `<data>/token`
//!  * no endpoint executes a shell command
//!  * no external CDN: the UI is three embedded files

use crate::engine::{safety_json, status_json, Action, Shared};
use crate::json::{self, Json};
use crate::types::RunMode;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Request, Response, Server, ServerLimits};

const MAX_BODY: usize = 64 * 1024;
const HTTP_WORKERS: usize = 4;
const MAX_EXPORT_BYTES: u64 = 16 * 1024 * 1024;
// Parse network input before this lock. Only configuration/action commits are
// serialized; a slow client must never hold the mutation lock while reading.
static MUTATIONS: Mutex<()> = Mutex::new(());

fn limits() -> ServerLimits {
    ServerLimits {
        max_connections: 8,
        connection_timeout: Duration::from_secs(5),
        max_header_bytes: 16 * 1024,
        max_connection_bytes: 256 * 1024,
    }
}

pub fn serve(
    shared: Arc<Shared>,
    bind: &str,
    port: u16,
) -> std::io::Result<thread::JoinHandle<()>> {
    let addr = bind_address(bind, port)?;
    let server = Server::http_with_limits(addr, limits())
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    shared
        .logger
        .info("api", format!("listening on http://{addr}"));
    let handle = thread::spawn(move || {
        thread::scope(|scope| {
            for _ in 0..HTTP_WORKERS {
                let server = &server;
                let shared = &shared;
                scope.spawn(move || {
                    for request in server.incoming_requests() {
                        handle(request, shared);
                    }
                });
            }
        });
    });
    Ok(handle)
}

fn bind_address(bind: &str, port: u16) -> std::io::Result<SocketAddr> {
    // Preserve the existing localhost configuration without performing DNS.
    let ip = if bind.eq_ignore_ascii_case("localhost") {
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    } else {
        bind.parse::<IpAddr>().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "API bind must be a loopback IP",
            )
        })?
    };
    if !ip.is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "API is loopback only",
        ));
    }
    Ok(SocketAddr::new(ip, port))
}

fn handle(mut request: Request, shared: &Arc<Shared>) {
    // Defence in depth: the listener is already bound to loopback, but a mis-edit of
    // `api.bind` must not expose the console to the network.
    let peer_ok = request
        .remote_addr()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false);
    if !peer_ok {
        return respond(request, 403, "text/plain", "loopback only".into());
    }
    if !same_origin(&request) {
        return respond(request, 403, "text/plain", "invalid Host or Origin".into());
    }

    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };
    let method = request.method().clone();

    if method == tiny_http::Method::Options {
        return respond(request, 204, "text/plain", String::new());
    }

    // Public static assets never contain credentials. Users enter the root-owned
    // token in the console, which keeps it in page memory only.
    match path.as_str() {
        "/" | "/index.html" => {
            return respond(
                request,
                200,
                "text/html; charset=utf-8",
                crate::webui::INDEX_HTML.to_string(),
            );
        }
        "/app.js" => {
            return respond(
                request,
                200,
                "application/javascript",
                crate::webui::APP_JS.to_string(),
            )
        }
        "/style.css" => {
            return respond(
                request,
                200,
                "text/css",
                crate::webui::STYLE_CSS.to_string(),
            )
        }
        "/favicon.ico" => return respond(request, 204, "text/plain", String::new()),
        _ => {}
    }

    if !path.starts_with("/api/") {
        return respond(request, 404, "text/plain", "not found".into());
    }

    if !authorized(shared, &request) {
        return respond(
            request,
            401,
            "application/json",
            r#"{"error":"unauthorized"}"#.into(),
        );
    }

    let limit = param(&query, "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(120)
        .min(2000);

    if path == "/api/export.csv" && method == tiny_http::Method::Get {
        let file = shared.paths.telemetry().join("history.csv");
        let body = match std::fs::File::open(file) {
            Ok(file) => {
                let mut body = String::new();
                if file
                    .take(MAX_EXPORT_BYTES + 1)
                    .read_to_string(&mut body)
                    .is_err()
                {
                    return respond(request, 500, "text/plain", "cannot read telemetry".into());
                }
                if body.len() as u64 > MAX_EXPORT_BYTES {
                    return respond(
                        request,
                        413,
                        "text/plain",
                        "telemetry export exceeds 16 MiB".into(),
                    );
                }
                body
            }
            Err(_) => "ts_iso,state\n# no telemetry yet\n".into(),
        };
        return respond(request, 200, "text/csv; charset=utf-8", body);
    }

    let body = match read_body(&mut request) {
        Ok(body) => body,
        Err((code, message)) => {
            let mut error = Json::obj();
            error.set("error", Json::str(message));
            return respond(request, code, "application/json", error.to_string());
        }
    };

    let mutation = if method == tiny_http::Method::Post {
        Some(MUTATIONS.lock().unwrap_or_else(|error| error.into_inner()))
    } else {
        None
    };
    let result: Result<(u16, String), String> = match (method.as_str(), path.as_str()) {
        ("GET", "/api/diagnostics") => Ok((
            200,
            shared.with(|d| crate::engine::diagnostics_json(d).to_string()),
        )),
        ("GET", "/api/status") => Ok((200, shared.with(|d| status_json(d).to_string()))),
        ("GET", "/api/policy") => Ok((
            200,
            shared.with(|d| {
                let mut j = d.decision.to_json();
                j.set(
                    "transitions",
                    Json::Arr(
                        d.transitions
                            .iter()
                            .map(|(ts, from, to, why)| {
                                let mut o = Json::obj();
                                o.set("ts", Json::num(*ts as f64));
                                o.set("from", Json::str(from.clone()));
                                o.set("to", Json::str(to.clone()));
                                o.set("reason", Json::str(why.clone()));
                                o
                            })
                            .collect(),
                    ),
                );
                j.set(
                    "config",
                    d.config
                        .to_json()
                        .get("policy")
                        .cloned()
                        .unwrap_or(Json::Null),
                );
                j.to_string()
            }),
        )),
        ("GET", "/api/cpu") => Ok((
            200,
            shared.with(|d| {
                let s = status_json(d);
                s.get("cpu").cloned().unwrap_or(Json::Null).to_string()
            }),
        )),
        ("GET", "/api/memory") => Ok((
            200,
            shared.with(|d| {
                let s = status_json(d);
                let mut out = Json::obj();
                out.set("memory", s.get("memory").cloned().unwrap_or(Json::Null));
                out.set("psi", s.get("psi").cloned().unwrap_or(Json::Null));
                out.set("policy", d.decision.to_json());
                out.to_string()
            }),
        )),
        ("GET", "/api/predict") => Ok((
            200,
            shared.with(|d| {
                let mut j = d.prediction.to_json();
                j.set("policy_preview", d.prediction_preview.to_json());
                j.set("stats", d.predictor_stats.to_json());
                j.set(
                    "usage",
                    Json::Arr(
                        d.usage
                            .iter()
                            .map(|(pkg, count)| {
                                let mut o = Json::obj();
                                o.set("package", Json::str(pkg.clone()));
                                o.set("count", Json::num(*count as f64));
                                o
                            })
                            .collect(),
                    ),
                );
                j.to_string()
            }),
        )),
        ("GET", "/api/safety") => Ok((200, shared.with(|d| safety_json(d).to_string()))),
        ("GET", "/api/learner") => Ok((200, shared.with(|d| d.learner.to_json().to_string()))),
        ("GET", "/api/logs") => {
            let entries = shared.logger.recent(limit);
            Ok((
                200,
                Json::Arr(entries.iter().map(|e| e.to_json()).collect()).to_string(),
            ))
        }
        ("GET", "/api/history") => Ok((
            200,
            shared.with(|d| {
                let n = d.history.len().min(limit);
                Json::Arr(
                    d.history
                        .iter()
                        .skip(d.history.len().saturating_sub(n))
                        .map(|p| p.to_json())
                        .collect(),
                )
                .to_string()
            }),
        )),
        ("GET", "/api/journal") => Ok((
            200,
            shared.with(|d| Json::Arr(d.journal.iter().map(|e| e.to_json()).collect()).to_string()),
        )),
        ("GET", "/api/config") => Ok((200, shared.with(|d| d.config.to_json().to_string()))),
        ("GET", "/api/capability") => Ok((
            200,
            shared.with(|d| {
                let mut j = Json::obj();
                j.set("actuator", d.capability.clone());
                j.set("device", d.device.to_json());
                j.set("thermal", d.thermal_json());
                j.set(
                    "knobs",
                    Json::Obj(
                        d.snapshot
                            .cpu
                            .knobs
                            .iter()
                            .map(|(k, v)| (k.clone(), Json::str(v.clone())))
                            .collect(),
                    ),
                );
                j.set(
                    "collectors",
                    Json::Arr(
                        d.snapshot
                            .health
                            .collectors
                            .iter()
                            .map(|c| {
                                let mut o = Json::obj();
                                o.set("name", Json::str(c.name.clone()));
                                o.set("ok", Json::Bool(c.ok));
                                o.set("note", Json::str(c.note.clone()));
                                o
                            })
                            .collect(),
                    ),
                );
                j.to_string()
            }),
        )),
        ("POST", "/api/action") => handle_action(shared, &body),
        ("POST", "/api/config") => handle_config(shared, &body),
        _ => Ok((404, r#"{"error":"unknown endpoint"}"#.to_string())),
    };

    drop(mutation);
    match result {
        Ok((code, payload)) => respond(request, code, "application/json", payload),
        Err(err) => {
            let mut j = Json::obj();
            j.set("error", Json::str(err));
            respond(request, 400, "application/json", j.to_string());
        }
    }
}

fn read_body(request: &mut Request) -> Result<String, (u16, &'static str)> {
    let lengths: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Content-Length"))
        .collect();
    let encodings: Vec<_> = request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Transfer-Encoding"))
        .collect();
    if lengths.len() > 1 || encodings.len() > 1 || (!lengths.is_empty() && !encodings.is_empty()) {
        return Err((400, "ambiguous body framing"));
    }
    if encodings
        .first()
        .is_some_and(|h| !h.value.as_str().trim().eq_ignore_ascii_case("chunked"))
    {
        return Err((400, "unsupported transfer encoding"));
    }
    let declared = lengths
        .first()
        .map(|h| h.value.as_str().trim().parse::<usize>())
        .transpose()
        .map_err(|_| (400, "invalid content length"))?;
    if declared.is_some_and(|length| length > MAX_BODY) {
        return Err((413, "request body too large"));
    }
    let mut buf = Vec::with_capacity(declared.unwrap_or(0).min(4096));
    request
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                (408, "request deadline exceeded")
            }
            _ => (400, "incomplete or invalid request body"),
        })?;
    if buf.len() > MAX_BODY {
        return Err((413, "request body too large"));
    }
    if declared.is_some_and(|length| length != buf.len()) {
        return Err((400, "incomplete request body"));
    }
    String::from_utf8(buf).map_err(|_| (400, "request body is not UTF-8"))
}

fn handle_action(shared: &Arc<Shared>, body: &str) -> Result<(u16, String), String> {
    let j = json::parse(body).map_err(|e| format!("bad json: {e}"))?;
    let action = j
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("missing action")?;
    match action {
        "rollback" => {
            shared.request_action(Action::Rollback);
            Ok(ok("rollback requested"))
        }
        "reset_model" => {
            shared.request_action(Action::ResetModel);
            Ok(ok("model reset requested"))
        }
        "predict" => {
            shared.request_action(Action::PredictNow);
            Ok(ok("prediction requested"))
        }
        "reload" => {
            shared.request_reload();
            Ok(ok("reload requested"))
        }
        "mode" => {
            let mode = j
                .get("value")
                .and_then(|v| v.as_str())
                .and_then(RunMode::parse)
                .ok_or("bad mode")?;
            set_mode(shared, mode)?;
            Ok(ok(&format!("mode set to {}", mode.as_str())))
        }
        other => Err(format!("unknown action {other}")),
    }
}

fn set_mode(shared: &Arc<Shared>, mode: RunMode) -> Result<(), String> {
    let mut cfg = mutation_config(shared)?;
    check_mode(shared, mode)?;
    cfg.mode = mode;
    cfg.save(&shared.paths.config())?;
    shared.request_reload();
    shared.logger.warn(
        "api",
        format!("mode changed to {} via WebUI", mode.as_str()),
    );
    Ok(())
}

fn mutation_config(shared: &Arc<Shared>) -> Result<crate::config::Config, String> {
    let path = shared.paths.config();
    if path.exists() {
        crate::config::Config::load(&path)
    } else {
        Ok(shared.with(|data| data.config.clone()))
    }
}

fn check_mode(shared: &Arc<Shared>, mode: RunMode) -> Result<(), String> {
    if mode == RunMode::Enabled {
        return Err("ACTUATOR_ENABLED=false; available modes are observe_only and dry_run".into());
    }
    if shared.with(|d| d.safe_mode) && mode.writes_allowed() {
        return Err("safe mode is active; writes stay disabled".into());
    }
    if shared.with(|d| d.thermal_fail_closed) && mode.writes_allowed() {
        return Err(
            "thermal fail-closed is active (sensor trust lost); writes stay disabled".into(),
        );
    }
    Ok(())
}

fn handle_config(shared: &Arc<Shared>, body: &str) -> Result<(u16, String), String> {
    let patch = json::parse(body).map_err(|e| format!("bad json: {e}"))?;
    if !matches!(patch, Json::Obj(_)) {
        return Err("configuration patch must be an object".into());
    }
    if patch.get_path("actuator.enabled").and_then(Json::as_bool) == Some(true) {
        return Err(
            "ACTUATOR_ENABLED=false; actuator activation requires device validation".into(),
        );
    }
    let mut cfg = mutation_config(shared)?;

    if let Some(mode) = patch
        .get("mode")
        .and_then(|v| v.as_str())
        .and_then(RunMode::parse)
    {
        check_mode(shared, mode)?;
        cfg.mode = mode;
    }
    macro_rules! setf {
        ($path:expr, $field:expr) => {
            if let Some(value) = patch.get_path($path) {
                $field = value
                    .as_f64()
                    .ok_or_else(|| format!("{} must be a finite number", $path))?;
            }
        };
    }
    setf!(
        "policy.thermal.enter_light_c",
        cfg.policy.thermal.enter_light_c
    );
    setf!(
        "policy.thermal.enter_moderate_c",
        cfg.policy.thermal.enter_moderate_c
    );
    setf!(
        "policy.thermal.enter_severe_c",
        cfg.policy.thermal.enter_severe_c
    );
    setf!(
        "policy.thermal.enter_critical_c",
        cfg.policy.thermal.enter_critical_c
    );
    setf!(
        "policy.thermal.exit_margin_c",
        cfg.policy.thermal.exit_margin_c
    );
    setf!(
        "policy.memory.psi_some_enter",
        cfg.policy.memory.psi_some_enter
    );
    setf!(
        "policy.memory.psi_some_exit",
        cfg.policy.memory.psi_some_exit
    );
    setf!(
        "policy.memory.psi_full_enter",
        cfg.policy.memory.psi_full_enter
    );
    setf!(
        "policy.memory.avail_pct_enter",
        cfg.policy.memory.avail_pct_enter
    );
    setf!("policy.battery.saver_pct", cfg.policy.battery.saver_pct);
    if let Some(v) = patch
        .get_path("policy.app_launch.boost_ms")
        .and_then(|v| v.as_u64())
    {
        cfg.policy.app_launch.boost_ms = v;
    }
    setf!("predictor.min_confidence", cfg.predictor.min_confidence);
    if let Some(v) = patch
        .get_path("predictor.min_samples")
        .and_then(|v| v.as_u64())
    {
        cfg.predictor.min_samples = v;
    }

    if let Some(v) = patch
        .get_path("policy.thermal.include_skin")
        .and_then(|v| v.as_bool())
    {
        cfg.policy.thermal.include_skin = v;
    }
    if let Some(v) = patch
        .get_path("predictor.enabled")
        .and_then(|v| v.as_bool())
    {
        cfg.predictor.enabled = v;
    }
    if let Some(v) = patch
        .get_path("actuator.enable_uclamp_top_app")
        .and_then(|v| v.as_bool())
    {
        cfg.actuator.enable_uclamp_top_app = v;
    }
    if let Some(list) = patch.get_path("policy.game.packages") {
        cfg.policy.game.packages = list
            .as_arr()
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(obj) = patch.get_path("actuator.profiles") {
        for state in crate::types::PolicyState::all() {
            if let Some(p) = obj.get(state.as_str()) {
                if let Some(slot) = cfg.actuator.profiles.iter_mut().find(|(s, _)| *s == state) {
                    if let Some(v) = p.get("uclamp_min").and_then(|v| v.as_u64()) {
                        slot.1.uclamp_min = v as u32;
                    }
                    if let Some(v) = p.get("uclamp_max").and_then(|v| v.as_u64()) {
                        slot.1.uclamp_max = v as u32;
                    }
                }
            }
        }
    }

    cfg.validate();
    cfg.save(&shared.paths.config())?;
    shared.request_reload();
    shared.logger.info("api", "config updated via WebUI");
    Ok(ok("config saved"))
}

fn ok(msg: &str) -> (u16, String) {
    let mut j = Json::obj();
    j.set("ok", Json::Bool(true));
    j.set("message", Json::str(msg));
    (200, j.to_string())
}

fn authorized(shared: &Arc<Shared>, request: &Request) -> bool {
    if shared.token.is_empty() {
        return false;
    }
    // Keep existing header-based clients compatible. Query-string credentials are
    // deliberately unsupported because URLs are retained in history and logs.
    for header in request.headers() {
        if header.field.equiv("X-NeuroSched-Token")
            && constant_time_eq(header.value.as_str().trim(), &shared.token)
        {
            return true;
        }
        if header.field.equiv("Authorization") {
            if let Some(t) = header.value.as_str().strip_prefix("Bearer ") {
                if constant_time_eq(t.trim(), &shared.token) {
                    return true;
                }
            }
        }
    }
    false
}

fn same_origin(request: &Request) -> bool {
    let mut hosts = request.headers().iter().filter(|h| h.field.equiv("Host"));
    let Some(host) = hosts.next() else {
        return false;
    };
    let host = host.value.as_str().trim();
    if hosts.next().is_some() || !loopback_authority(host) {
        return false;
    }
    let expected = format!("http://{host}");
    let mut origins = request.headers().iter().filter(|h| h.field.equiv("Origin"));
    match origins.next() {
        Some(origin) => {
            origins.next().is_none() && origin.value.as_str().trim().eq_ignore_ascii_case(&expected)
        }
        None => true,
    }
}

fn loopback_authority(authority: &str) -> bool {
    if authority.eq_ignore_ascii_case("localhost")
        || authority == "[::1]"
        || authority
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    {
        return true;
    }
    if let Some((host, port)) = authority.rsplit_once(':') {
        if host.eq_ignore_ascii_case("localhost") {
            return port.parse::<u16>().map(|n| n > 0).unwrap_or(false);
        }
    }
    authority
        .parse::<SocketAddr>()
        .map(|addr| addr.ip().is_loopback() && addr.port() > 0)
        .unwrap_or(false)
}

fn param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn respond(request: Request, code: u16, ctype: &str, body: String) {
    let mut response = Response::from_string(body).with_status_code(code);
    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()) {
        response = response.with_header(h);
    }
    if let Ok(h) = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]) {
        response = response.with_header(h);
    }
    if let Ok(h) = Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]) {
        response = response.with_header(h);
    }
    if let Ok(h) = Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..]) {
        response = response.with_header(h);
    }
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests_transport;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Data;
    use crate::storage::{Level, Logger, Paths};
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    pub(super) fn shared(token: &str) -> Arc<Shared> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let paths = Paths::new(std::env::temp_dir().join(format!(
            "neurosched-api-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        paths.ensure().unwrap();
        Arc::new(Shared {
            token: token.into(),
            started_ms: 0,
            version: crate::VERSION.into(),
            logger: Arc::new(Logger::new(
                100,
                Level::Info,
                paths.logs().join("api.log"),
                64,
            )),
            paths,
            data: Mutex::new(Data::default()),
        })
    }

    /// Exercise tiny_http request parsing and the production handler through TCP.
    /// A bounded one-request server avoids leaving listener threads after a test.
    fn http(shared: Arc<Shared>, method: &str, path: &str, headers: &str, body: &str) -> String {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let worker = thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            handle(request, &shared);
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let headers = headers.replace("__LOCAL_ORIGIN__", &format!("http://{addr}"));
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        response
    }

    #[test]
    fn homepage_does_not_disclose_api_token() {
        let secret = "private-api-test-credential";
        let response = http(shared(secret), "GET", "/", "", "");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains(secret));
        assert!(!response.contains("__NEUROSCHED_TOKEN__"));
        assert!(response.contains("type=\"password\""));
        assert!(response.contains("ACTUATOR_ENABLED=false"));
    }

    #[test]
    fn api_requires_header_credential_and_rejects_empty_secret() {
        let state = shared("credential");
        for (path, headers) in [
            ("/api/status", ""),
            ("/api/status?token=credential", ""),
            ("/api/status", "X-NeuroSched-Token: wrong\r\n"),
        ] {
            let response = http(state.clone(), "GET", path, headers, "");
            assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        }
        let response = http(
            shared(""),
            "GET",
            "/api/status",
            "X-NeuroSched-Token: \r\n",
            "",
        );
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    }

    #[test]
    fn legacy_token_and_bearer_headers_authorize_requests() {
        for headers in [
            "X-NeuroSched-Token: credential\r\n",
            "Authorization: Bearer credential\r\n",
        ] {
            let response = http(shared("credential"), "GET", "/api/status", headers, "");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.contains("\"mode\":\"observe_only\""));
        }
    }

    #[test]
    fn same_origin_browser_request_is_accepted() {
        let response = http(
            shared("credential"),
            "POST",
            "/api/action",
            "X-NeuroSched-Token: credential\r\nOrigin: __LOCAL_ORIGIN__\r\n",
            r#"{"action":"predict"}"#,
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    }

    #[test]
    fn foreign_origin_is_rejected_before_authenticated_action() {
        let shared = shared("credential");
        let response = http(
            shared.clone(),
            "POST",
            "/api/action",
            "X-NeuroSched-Token: credential\r\nOrigin: https://foreign.example\r\n",
            r#"{"action":"rollback"}"#,
        );
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(shared.with(|d| d.action.is_none()));
    }

    #[test]
    fn invalid_host_is_rejected_before_serving_homepage() {
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        let shared = shared("credential");
        let worker = thread::spawn(move || {
            handle(
                server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap(),
                &shared,
            );
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: foreign.example\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }

    #[test]
    fn non_finite_config_returns_error_without_panic_or_save() {
        let shared = shared("credential");
        let response = http(
            shared.clone(),
            "POST",
            "/api/config",
            "X-NeuroSched-Token: credential\r\nContent-Type: application/json\r\n",
            r#"{"policy":{"thermal":{"enter_light_c":"NaN"}}}"#,
        );
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(response.contains("finite number"));
        assert!(!shared.paths.config().exists());
        assert!(!shared.with(|d| d.reload_requested));
    }

    #[test]
    fn api_cannot_activate_actuator() {
        for (path, body) in [
            ("/api/action", r#"{"action":"mode","value":"enabled"}"#),
            ("/api/config", r#"{"mode":"enabled"}"#),
            ("/api/config", r#"{"actuator":{"enabled":true}}"#),
        ] {
            let shared = shared("credential");
            let response = http(
                shared.clone(),
                "POST",
                path,
                "X-NeuroSched-Token: credential\r\n",
                body,
            );
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
            assert!(response.contains("ACTUATOR_ENABLED=false"));
            assert!(!shared.paths.config().exists());
        }
    }

    #[test]
    fn accepted_authorities_are_loopback_only() {
        for authority in [
            "localhost",
            "localhost:8787",
            "127.0.0.1:8787",
            "[::1]",
            "[::1]:8787",
        ] {
            assert!(loopback_authority(authority), "{authority}");
        }
        for authority in [
            "foreign.example",
            "127.0.0.1.foreign.example",
            "localhost:0",
            "127.0.0.1:65536",
            "192.168.1.1:8787",
            "user@localhost:8787",
        ] {
            assert!(!loopback_authority(authority), "{authority}");
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn query_param_extraction() {
        assert_eq!(param("token=abc&limit=5", "token").as_deref(), Some("abc"));
        assert_eq!(param("limit=5", "missing"), None);
    }

    #[test]
    fn token_comparison_is_length_aware() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
