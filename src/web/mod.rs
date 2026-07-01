//! Web 服务：内嵌单页 + 只读 JSON API（查 SQLite）。
use crate::db;
use axum::{
    extract::{Query, State},
    response::{Html, Json},
    routing::get,
    Router,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    db_path: Arc<String>,
}

pub async fn run(port: u16, db_path: String) {
    let state = AppState {
        db_path: Arc::new(db_path),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/opportunities", get(api_opps))
        .route("/api/stats", get(api_stats))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => {
            info!("Web 服务监听 http://127.0.0.1:{port}");
            if let Err(e) = axum::serve(listener, app).await {
                error!(error = %e, "Web 服务异常退出");
            }
        }
        Err(e) => error!(error = %e, "Web 端口绑定失败 {addr}"),
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

fn parse_filters(q: &HashMap<String, String>) -> db::Filters {
    db::Filters {
        from: q.get("from").and_then(|v| v.parse().ok()),
        to: q.get("to").and_then(|v| v.parse().ok()),
        kind: q.get("kind").filter(|v| !v.is_empty()).cloned(),
        status: q.get("status").filter(|v| !v.is_empty()).cloned(),
        expiry: q.get("expiry").filter(|v| !v.is_empty()).cloned(),
        min_rate: q.get("min_rate").and_then(|v| v.parse().ok()),
        limit: q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(500),
    }
}

async fn api_opps(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let path = (*st.db_path).clone();
    let filters = parse_filters(&q);
    let res = tokio::task::spawn_blocking(move || db::query(&path, &filters)).await;
    match res {
        Ok(Ok(rows)) => Json(serde_json::json!({ "ok": true, "rows": rows })),
        Ok(Err(e)) => Json(serde_json::json!({ "ok": false, "error": e.to_string(), "rows": [] })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string(), "rows": [] })),
    }
}

async fn api_stats(State(st): State<AppState>) -> Json<serde_json::Value> {
    let path = (*st.db_path).clone();
    let res = tokio::task::spawn_blocking(move || db::stats(&path)).await;
    match res {
        Ok(Ok(v)) => Json(v),
        _ => Json(serde_json::json!({ "total": 0 })),
    }
}
