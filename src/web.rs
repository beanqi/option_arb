use crate::models::{OpportunityRecord, Snapshot};
use axum::{
    Json, Router,
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
};
use std::sync::Arc;
use tokio::sync::RwLock;

const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[derive(Clone)]
pub struct AppState {
    pub snapshot: Arc<RwLock<Snapshot>>,
    pub history: Arc<RwLock<Vec<OpportunityRecord>>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/snapshot", get(snapshot))
        .route("/api/history", get(history))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

async fn snapshot(State(state): State<AppState>) -> Json<Snapshot> {
    Json(state.snapshot.read().await.clone())
}

async fn history(State(state): State<AppState>) -> Json<Vec<OpportunityRecord>> {
    Json(state.history.read().await.clone())
}
