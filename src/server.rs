use crate::{
    collect,
    store::{ImportRequest, Query, Store},
};
use anyhow::{Context, Result};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use std::{
    io::Read,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub struct State {
    pub store: Mutex<Store>,
    pub scanning: AtomicBool,
    pub home: PathBuf,
    pub extra: Vec<PathBuf>,
}
impl State {
    pub fn scan(self: &Arc<Self>) -> bool {
        if self
            .scanning
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        let state = self.clone();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(|| collect::scan(&state.home, &state.extra));
            match result {
                Ok(scan) => {
                    if let Ok(mut db) = state.store.lock() {
                        if db.apply_scan(scan).is_err() {
                            eprintln!("Could not save scan; previous numeric history retained.");
                        }
                    }
                }
                Err(_) => eprintln!("Collector failed; previous numeric history retained."),
            }
            state.scanning.store(false, Ordering::SeqCst);
        });
        true
    }
}

fn header<'a>(r: &'a Request, key: &'static str) -> Option<&'a str> {
    r.headers()
        .iter()
        .find(|h| h.field.equiv(key))
        .map(|h| h.value.as_str())
}
pub fn authorized(
    host: &str,
    origin: Option<&str>,
    fetch_site: Option<&str>,
    auth: Option<&str>,
    expected_host: &str,
    token: &str,
    post: bool,
) -> bool {
    if host != expected_host {
        return false;
    }
    let expected_origin = format!("http://{expected_host}");
    if origin.is_some_and(|o| o != expected_origin)
        || fetch_site.is_some_and(|s| !["same-origin", "none"].contains(&s))
    {
        return false;
    }
    if post && origin != Some(expected_origin.as_str()) {
        return false;
    }
    crate::safety::constant_time_equal(auth.unwrap_or(""), &format!("Bearer {token}"))
}

fn respond(req: Request, status: u16, content_type: &str, body: Vec<u8>) {
    let headers=[("Content-Type",content_type),("Cache-Control","no-store"),("X-Content-Type-Options","nosniff"),("Referrer-Policy","no-referrer"),("X-Frame-Options","DENY"),("Content-Security-Policy","default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'")];
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    for (k, v) in headers {
        response.add_header(Header::from_bytes(k, v).unwrap());
    }
    let _ = req.respond(response);
}
fn json_response(req: Request, status: u16, value: Value) {
    respond(
        req,
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec(&value).unwrap_or_default(),
    );
}

fn handle(mut req: Request, state: &Arc<State>, host: &str, token: &str) {
    if header(&req, "Host") != Some(host) {
        json_response(req, 403, json!({"error":"Invalid Host"}));
        return;
    }
    let raw = req.url().to_owned();
    let (path, query) = raw.split_once('?').unwrap_or((&raw, ""));
    if !path.starts_with("/api/") {
        if req.method() != &Method::Get {
            json_response(req, 405, json!({"error":"Method not allowed"}));
            return;
        }
        match path {
            "/" | "/index.html" => respond(
                req,
                200,
                "text/html; charset=utf-8",
                include_bytes!("../web/index.html").to_vec(),
            ),
            "/app.js" => respond(
                req,
                200,
                "text/javascript; charset=utf-8",
                include_bytes!("../web/app.js").to_vec(),
            ),
            "/style.css" => respond(
                req,
                200,
                "text/css; charset=utf-8",
                include_bytes!("../web/style.css").to_vec(),
            ),
            "/favicon.ico" => respond(req, 204, "image/x-icon", vec![]),
            _ => json_response(req, 404, json!({"error":"Not found"})),
        }
        return;
    }
    let post = req.method() == &Method::Post;
    if !authorized(
        header(&req, "Host").unwrap_or(""),
        header(&req, "Origin"),
        header(&req, "Sec-Fetch-Site"),
        header(&req, "Authorization"),
        host,
        token,
        post,
    ) {
        json_response(
            req,
            403,
            json!({"error":"Access denied. Open the dashboard using the launch URL."}),
        );
        return;
    }
    if req.method() == &Method::Get {
        let query = match Query::parse(query) {
            Ok(q) => q,
            Err(_) => {
                json_response(
                    req,
                    400,
                    json!({"error":"Invalid date range, timezone, or grain"}),
                );
                return;
            }
        };
        let result = (|| -> Result<Value> {
            let db = state
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("Database unavailable"))?;
            match path {
                "/api/summary" => db.summary(&query),
                "/api/timeseries" => db.timeseries(&query),
                "/api/coverage" => {
                    let mut v = db.coverage()?;
                    v["scanning"] = json!(state.scanning.load(Ordering::SeqCst));
                    Ok(v)
                }
                "/api/export" => db.export(),
                _ => Ok(json!({"error":"Not found"})),
            }
        })();
        match result {
            Ok(v) => {
                let status = if v.get("error").is_some() { 404 } else { 200 };
                json_response(req, status, v)
            }
            Err(_) => json_response(req, 500, json!({"error":"Could not read numeric ledger"})),
        }
        return;
    }
    if !post {
        json_response(req, 405, json!({"error":"Method not allowed"}));
        return;
    }
    if path == "/api/scan" {
        let started = state.scan();
        json_response(req, 202, json!({"scanning":true,"started":started}));
        return;
    }
    if path != "/api/import" {
        json_response(req, 404, json!({"error":"Not found"}));
        return;
    }
    if !header(&req, "Content-Type")
        .is_some_and(|s| s.split(';').next() == Some("application/json"))
    {
        json_response(req, 415, json!({"error":"Expected application/json"}));
        return;
    }
    const MAX: u64 = 64 * 1024 * 1024;
    if req.body_length().is_some_and(|n| n as u64 > MAX) {
        json_response(req, 413, json!({"error":"Request exceeds 64 MiB"}));
        return;
    }
    let mut body = Vec::new();
    if req
        .as_reader()
        .take(MAX + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        json_response(req, 400, json!({"error":"Could not read import"}));
        return;
    }
    if body.len() as u64 > MAX {
        json_response(req, 413, json!({"error":"Request exceeds 64 MiB"}));
        return;
    }
    let parsed: ImportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            json_response(req, 400, json!({"error":"Invalid import fields"}));
            return;
        }
    };
    drop(body);
    let result = state
        .store
        .lock()
        .map_err(|_| anyhow::anyhow!("Database unavailable"))
        .and_then(|mut s| s.import(&parsed));
    match result {
        Ok(value) => json_response(req, 200, value),
        Err(e) => json_response(req, 400, json!({"error":format!("{e}")})),
    }
}

pub fn serve(
    store: Store,
    home: PathBuf,
    extra: Vec<PathBuf>,
    port: u16,
    open: bool,
) -> Result<()> {
    let server = Server::http((std::net::Ipv4Addr::LOCALHOST, port))
        .map_err(|e| anyhow::anyhow!("Could not bind loopback server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("Missing server address")?
        .port();
    let host = format!("127.0.0.1:{port}");
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let url = format!("http://{host}/#token={token}");
    let state = Arc::new(State {
        store: Mutex::new(store),
        scanning: AtomicBool::new(false),
        home,
        extra,
    });
    state.scan();
    let background = state.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(300));
        background.scan();
    });
    println!("TokenLedger — offline, read-only collection\n{url}\nKeep this URL private. Stop the server with Ctrl+C.");
    if open {
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(&url)
                .spawn();
        }
        #[cfg(not(target_os = "macos"))]
        eprintln!("Open the launch URL in your browser.");
    }
    for request in server.incoming_requests() {
        handle(request, &state, &host, &token);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unauthenticated_cross_origin_and_rebinding() {
        let host = "127.0.0.1:4317";
        let good = Some("Bearer secret");
        assert!(authorized(
            host,
            None,
            Some("same-origin"),
            good,
            host,
            "secret",
            false
        ));
        assert!(!authorized(host, None, None, None, host, "secret", false));
        assert!(!authorized(
            "evil.test:4317",
            None,
            None,
            good,
            host,
            "secret",
            false
        ));
        assert!(!authorized(
            host,
            Some("https://evil.test"),
            None,
            good,
            host,
            "secret",
            false
        ));
        assert!(!authorized(host, None, None, good, host, "secret", true));
        assert!(authorized(
            host,
            Some("http://127.0.0.1:4317"),
            Some("same-origin"),
            good,
            host,
            "secret",
            true
        ));
    }
}
