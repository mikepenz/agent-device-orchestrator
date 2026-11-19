//! `devq live`: tiny read-only HTTP server (127.0.0.1) for the dashboard.
use crate::{db, pool};
use db::{now_ms, Res};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Response, Server};

const PAGE: &str = include_str!("../design/live.html");

/// Embedded static assets: path -> (content type, bytes).
const ASSETS: [(&str, &str, &[u8]); 6] = [
    ("/manifest.webmanifest", "application/manifest+json", include_bytes!("../design/manifest.webmanifest")),
    ("/icon.svg", "image/svg+xml", include_bytes!("../design/icon.svg")),
    ("/icon-192.png", "image/png", include_bytes!("../design/icon-192.png")),
    ("/icon-512.png", "image/png", include_bytes!("../design/icon-512.png")),
    ("/icon-maskable-512.png", "image/png", include_bytes!("../design/icon-maskable-512.png")),
    ("/sw.js", "text/javascript", include_bytes!("../design/sw.js")),
];

fn hdr(k: &str, v: &str) -> Header {
    Header::from_bytes(k, v).unwrap()
}

fn plain(code: u16, msg: &str) -> tiny_http::ResponseBox {
    Response::from_string(msg)
        .with_status_code(code)
        .with_header(hdr("Content-Type", "text/plain; charset=utf-8"))
        .with_header(hdr("X-Content-Type-Options", "nosniff"))
        .boxed()
}

fn state(conn: &mut rusqlite::Connection, home: &std::path::Path, window: i64) -> Res<String> {
    let cfg = pool::load_config(home)?; // per request: config edits apply without restart
    pool::sync(conn, now_ms(), &cfg)?;
    let now = now_ms(); // after sync (adb can be slow), right before the tx
    let window = window.clamp(1, 604_800); // bounds write-lock time and response size
    let mut st = db::immediate(conn, |c| {
        db::tick(c, now)?;
        db::state_json(c, now, window)
    })?;
    pool::filter_state(&mut st, &cfg);
    Ok(st.to_string())
}

pub fn serve(port: u16, open: bool) -> Res<()> {
    let home = pool::devq_home();
    let mut conn = db::open(&home)?;
    let server = Server::http(("127.0.0.1", port)).map_err(|e| format!("cannot bind 127.0.0.1:{port}: {e}"))?;
    let url = format!("http://127.0.0.1:{port}/");
    println!("devq live: {url}");
    if open {
        crate::sys::open_url(&url);
    }
    let stop = Arc::new(AtomicBool::new(false));
    crate::sys::install_signals(&stop, &stop);
    // ponytail: sequential handling; a slow `adb devices` sync blocks other requests (it is throttled).
    while !stop.load(Relaxed) {
        let Some(req) = server.recv_timeout(Duration::from_millis(200))? else { continue };
        // ponytail: DNS-rebinding guard; a hostile page could resolve its own name to 127.0.0.1 and read /api/state.
        let host_ok = req.headers().iter().any(|h| {
            h.field.equiv("Host") && [format!("127.0.0.1:{port}"), format!("localhost:{port}")].contains(&h.value.as_str().to_string())
        });
        let (path, query) = req.url().split_once('?').unwrap_or((req.url(), ""));
        let resp = match path {
            _ if !host_ok => plain(403, "forbidden"),
            "/" => Response::from_string(PAGE)
                .with_header(hdr("Content-Type", "text/html; charset=utf-8"))
                .with_header(hdr("X-Content-Type-Options", "nosniff"))
                .boxed(),
            "/api/state" => {
                let window = query.split('&').find_map(|kv| kv.strip_prefix("window=")).and_then(|v| v.parse::<i64>().ok()).unwrap_or(3600);
                let (code, body) = match state(&mut conn, &home, window) {
                    Ok(b) => (200, b),
                    Err(e) => (500, serde_json::json!({"error": e.to_string()}).to_string()),
                };
                Response::from_string(body)
                    .with_status_code(code)
                    .with_header(hdr("Content-Type", "application/json"))
                    .with_header(hdr("Cache-Control", "no-store"))
                    .with_header(hdr("X-Content-Type-Options", "nosniff"))
                    .boxed()
            }
            p if ASSETS.iter().any(|a| a.0 == p) => {
                let (_, ct, body) = ASSETS.iter().find(|a| a.0 == p).unwrap();
                let r =
                    Response::from_data(*body).with_header(hdr("Content-Type", ct)).with_header(hdr("X-Content-Type-Options", "nosniff"));
                if p == "/sw.js" {
                    r.with_header(hdr("Service-Worker-Allowed", "/")).boxed()
                } else {
                    r.boxed()
                }
            }
            _ => plain(404, "not found"),
        };
        let _ = req.respond(resp);
    }
    Ok(())
}
