use super::upstream::{authenticate, json_response, Ctx};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;

pub fn build_router(ctx: Ctx) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .with_state(ctx)
}

async fn healthz() -> Response {
    json_response(
        StatusCode::OK,
        serde_json::json!({ "ok": true, "service": "workbuddy-codex-gateway", "version": "0.2.0" }),
    )
}

async fn list_models(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    let master_key = ctx.cfg.read().await.master_key.clone();
    if let Err(resp) = authenticate(&ctx, &headers, &master_key) {
        return resp;
    }
    let cfg = ctx.cfg.read().await.clone();
    let data: Vec<serde_json::Value> = cfg
        .models
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.codex_name.clone(),
                "object": "model",
                "created": 0,
                "owned_by": "workbuddy"
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        serde_json::json!({ "object": "list", "data": data }),
    )
}

async fn chat_completions(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    super::upstream::handle_chat(ctx, headers, body.to_vec()).await
}

async fn responses(State(ctx): State<Ctx>, headers: HeaderMap, body: Bytes) -> Response {
    super::upstream::handle_responses(ctx, headers, body.to_vec()).await
}
