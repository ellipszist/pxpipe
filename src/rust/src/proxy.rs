//! HTTP server: accept from Claude Code, transform, forward to api.anthropic.com.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use crate::transform::{transform, TransformConfig, TransformInfo};
use crate::AppState;

// Connection-specific (hop-by-hop) headers + things we always strip.
const HOP_HEADERS: &[&str] = &[
    "connection", "keep-alive", "proxy-connection", "transfer-encoding",
    "upgrade", "te", "host", "content-length", "expect", "accept-encoding",
];

#[derive(Clone, serde::Serialize)]
pub struct RequestEntry {
    pub method: String,
    pub path: String,
    pub size_in: usize,
    pub size_out: usize,
    pub status: u16,
    pub upstream_ms: u64,
    pub info: TransformInfo,
    pub tokens: Option<TokenLog>,
    pub session_saved_so_far: f64,
    pub session_saved_usd: f64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct TokenLog {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    pub effective_cost: f64,
}

// Tail of recent requests for the dashboard.
pub struct Recent(pub Mutex<VecDeque<RequestEntry>>);
impl Default for Recent {
    fn default() -> Self { Recent(Mutex::new(VecDeque::with_capacity(64))) }
}
impl Recent {
    pub fn push(&self, e: RequestEntry) {
        let mut q = self.0.lock().unwrap();
        if q.len() >= 64 { q.pop_front(); }
        q.push_back(e);
    }
    pub fn snapshot(&self) -> Vec<RequestEntry> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

pub async fn serve(port: u16, state: AppState) -> anyhow::Result<()> {
    let recent: std::sync::Arc<Recent> = std::sync::Arc::new(Recent::default());

    let app = Router::new()
        .route("/proxy-stats", get(stats_handler))
        .fallback(any(proxy_handler))
        .with_state((state, recent));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn stats_handler(
    State((state, _recent)): State<(AppState, std::sync::Arc<Recent>)>,
) -> impl IntoResponse {
    let snap = state.stats.snapshot();
    (StatusCode::OK, [("content-type", "application/json")],
     serde_json::to_string_pretty(&snap).unwrap_or_default())
}

async fn proxy_handler(
    State((state, recent)): State<(AppState, std::sync::Arc<Recent>)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or("/".to_string());
    let started = Instant::now();

    // Only POST /v1/messages gets compressed; everything else is passthrough.
    let do_compress = state.args.no_compress == false
        && method == Method::POST
        && path.starts_with("/v1/messages");

    let cfg = TransformConfig {
        compress: do_compress,
        compress_tools: !state.args.no_tools,
        compress_schemas: !state.args.no_schemas,
        compress_reminders: !state.args.no_reminders,
        compress_tool_results: !state.args.no_tool_results,
        min_chars: state.args.min_chars,
        font: &state.font,
        render_cache: &state.render_cache,
    };

    let (new_body, info) = transform(&body, &cfg);

    // Build the upstream URL
    let upstream_url = format!("{}{}", state.args.upstream, path);

    // Forward headers (strip hop-by-hop + content-length, reqwest sets its own)
    let mut fwd_headers = reqwest::header::HeaderMap::new();
    for (k, v) in headers.iter() {
        let lower = k.as_str().to_ascii_lowercase();
        if HOP_HEADERS.contains(&lower.as_str()) { continue; }
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            fwd_headers.insert(name, val);
        }
    }
    if !fwd_headers.contains_key("anthropic-version") {
        fwd_headers.insert(
            reqwest::header::HeaderName::from_static("anthropic-version"),
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
    }

    let rmethod = match method {
        Method::GET => reqwest::Method::GET,
        Method::POST => reqwest::Method::POST,
        Method::PUT => reqwest::Method::PUT,
        Method::DELETE => reqwest::Method::DELETE,
        Method::HEAD => reqwest::Method::HEAD,
        _ => reqwest::Method::POST,
    };

    let upstream_started = Instant::now();
    let upstream_result = state.client
        .request(rmethod, &upstream_url)
        .headers(fwd_headers)
        .body(new_body.clone())
        .send()
        .await;
    let upstream_ms = upstream_started.elapsed().as_millis() as u64;

    let upstream_resp = match upstream_result {
        Ok(r) => r,
        Err(e) => {
            let msg = format!(r#"{{"error":"upstream_error","detail":{}}}"#, serde_json::to_string(&e.to_string()).unwrap());
            return (StatusCode::BAD_GATEWAY, [("content-type", "application/json")], msg).into_response();
        }
    };

    let status = upstream_resp.status();
    let mut resp_headers = HeaderMap::new();
    for (k, v) in upstream_resp.headers() {
        let lower = k.as_str().to_ascii_lowercase();
        if HOP_HEADERS.contains(&lower.as_str()) || lower == "content-encoding" { continue; }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            resp_headers.insert(name, val);
        }
    }
    let resp_body = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(_) => Bytes::new(),
    };

    // Parse usage (plain JSON or SSE stream) for stats.
    let token_log = parse_usage(&resp_body);
    let (eff, baseline) = compute_effective_and_baseline(&token_log, &info);
    state.stats.record(eff, baseline, info.compressed);
    let snap = state.stats.snapshot();

    let entry = RequestEntry {
        method: method.as_str().to_string(),
        path: path.clone(),
        size_in: body.len(),
        size_out: resp_body.len(),
        status: status.as_u16(),
        upstream_ms,
        info: info.clone(),
        tokens: token_log.clone(),
        session_saved_so_far: snap.saved_effective_tokens,
        session_saved_usd: snap.saved_usd_opus47,
    };
    recent.push(entry.clone());

    // Single-line JSON log per request (matches the Python proxy log shape).
    if let Ok(j) = serde_json::to_string(&entry) {
        println!("[PROXY] {}", j);
    }

    let _ = started;
    (status, resp_headers, resp_body).into_response()
}

fn parse_usage(body: &[u8]) -> Option<TokenLog> {
    // Try plain JSON first
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        if let Some(u) = v.get("usage") {
            return Some(usage_to_log(u));
        }
    }
    // Try SSE: scan for "data: {...}" lines and aggregate.
    let text = std::str::from_utf8(body).ok()?;
    let mut inp: u64 = 0;
    let mut out: u64 = 0;
    let mut cr: u64 = 0;
    let mut cc: u64 = 0;
    let mut any = false;
    for line in text.lines() {
        let line = line.trim_start();
        let payload = match line.strip_prefix("data: ") {
            Some(p) => p,
            None => continue,
        };
        let ev: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match ev.get("type").and_then(|v| v.as_str()) {
            Some("message_start") => {
                if let Some(u) = ev.get("message").and_then(|m| m.get("usage")) {
                    inp = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(inp);
                    cr = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(cr);
                    cc = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(cc);
                    any = true;
                }
            }
            Some("message_delta") => {
                if let Some(u) = ev.get("usage") {
                    if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                        out = v;
                        any = true;
                    }
                }
            }
            _ => {}
        }
    }
    if any {
        let eff = inp as f64 + cc as f64 * 1.25 + cr as f64 * 0.10;
        Some(TokenLog { input: inp, output: out, cache_read: cr, cache_create: cc, effective_cost: eff })
    } else {
        None
    }
}

fn usage_to_log(u: &serde_json::Value) -> TokenLog {
    let inp = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let out = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cr = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cc = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let eff = inp as f64 + cc as f64 * 1.25 + cr as f64 * 0.10;
    TokenLog { input: inp, output: out, cache_read: cr, cache_create: cc, effective_cost: eff }
}

fn compute_effective_and_baseline(token_log: &Option<TokenLog>, info: &TransformInfo) -> (f64, f64) {
    let Some(t) = token_log else { return (0.0, 0.0); };
    let actual = t.effective_cost;
    if !info.compressed {
        return (actual, actual);
    }
    // Conservative baseline: assume the replaced text would have been cached
    // at 10% rate, and the extra char count is what we saved.
    let txt_replaced = ((info.system_text_chars + info.tool_text_added) / 4) as u64;
    let extra = txt_replaced.saturating_sub(info.expected_image_tokens);
    let baseline = actual + extra as f64 * 0.10;
    (actual, baseline)
}
