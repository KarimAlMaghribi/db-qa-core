use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use db_qa_core::config::{Config, PlannerMode};
use db_qa_core::index::{ensure_fresh_index, load_meta, rebuild_index, SchemaIndex, SCHEMA_VERSION};
use db_qa_core::ir::{compile_plan, Operation, ParamValue};
use db_qa_core::planner::plan_query;
use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgRow, PgPool, Row};
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use tracing::{error, info};

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    index: Arc<RwLock<SchemaIndex>>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    config: Config,
}

#[derive(Deserialize)]
struct QueryRequest {
    prompt: String,
    #[serde(default)]
    top_k: Option<usize>,
}

#[derive(Serialize)]
struct QueryResponse {
    sql: String,
    params: Vec<serde_json::Value>,
    data: Vec<serde_json::Value>,
    meta: serde_json::Value,
}

#[derive(Serialize)]
struct ReadyResponse {
    ready: bool,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env();
    let pool = PgPool::connect(&config.pg_url).await?;

    let last_error = Arc::new(Mutex::new(None));
    let index = match ensure_fresh_index(&config.sqlite_path, &pool).await {
        Ok(index) => index,
        Err(err) => {
            error!("index build failed: {err}");
            if let Ok(mut guard) = last_error.lock() {
                *guard = Some(err.to_string());
            }
            SchemaIndex::empty()
        }
    };

    let ready_flag = Arc::new(std::sync::atomic::AtomicBool::new(!index.tables.is_empty()));

    let state = AppState {
        pool,
        index: Arc::new(RwLock::new(index)),
        ready: ready_flag,
        last_error,
        config,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/index/rebuild", post(rebuild))
        .route("/index/status", get(index_status))
        .route("/query", post(query))
        .with_state(state.clone());

    let addr = format!("{}:{}", state.config.host, state.config.port);
    info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    StatusCode::OK
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let ready = state.ready.load(std::sync::atomic::Ordering::SeqCst);
    if ready {
        return (StatusCode::OK, Json(ReadyResponse { ready, error: None })).into_response();
    }
    let error = state.last_error.lock().ok().and_then(|value| value.clone());
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ReadyResponse { ready, error }),
    )
        .into_response()
}

async fn rebuild(State(state): State<AppState>) -> impl IntoResponse {
    match rebuild_index(&state.config.sqlite_path, &state.pool).await {
        Ok(index) => {
            let mut guard = state.index.write().await;
            let is_ready = !index.tables.is_empty();
            *guard = index;
            state.ready
                .store(is_ready, std::sync::atomic::Ordering::SeqCst);
            (StatusCode::OK, Json(serde_json::json!({"status": "rebuilt"}))).into_response()
        }
        Err(err) => {
            state.ready.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut guard) = state.last_error.lock() {
                *guard = Some(err.to_string());
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"status": "error", "error": err.to_string()})),
            )
                .into_response()
        }
    }
}

async fn index_status(State(state): State<AppState>) -> impl IntoResponse {
    let index = state.index.read().await;
    let meta = load_meta(&state.config.sqlite_path).unwrap_or_default();
    let last_built_at = meta.get("last_built_at").cloned();
    let fingerprint = if index.fingerprint.is_empty() {
        meta.get("fingerprint").cloned().unwrap_or_default()
    } else {
        index.fingerprint.clone()
    };
    let ready = state.ready.load(std::sync::atomic::Ordering::SeqCst);
    let response = serde_json::json!({
        "ready": ready,
        "fingerprint": fingerprint,
        "table_count": index.tables.len(),
        "sqlite_path": state.config.sqlite_path,
        "last_built_at": last_built_at,
        "schema_version": meta.get("schema_version").cloned().unwrap_or_else(|| SCHEMA_VERSION.to_string())
    });
    (StatusCode::OK, Json(response)).into_response()
}

async fn query(State(state): State<AppState>, Json(payload): Json<QueryRequest>) -> impl IntoResponse {
    let _ = payload.top_k;
    let index = state.index.read().await;
    if index.tables.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "index not ready"})),
        )
            .into_response();
    }

    let plan = match plan_query(&payload.prompt, &index, &state.config).await {
        Ok(plan) => plan,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };

    if plan.operation == Operation::ListTables {
        let mut tables = index.tables.values().collect::<Vec<_>>();
        tables.sort_by(|a, b| (a.schema.as_str(), a.name.as_str()).cmp(&(b.schema.as_str(), b.name.as_str())));
        let data = tables
            .into_iter()
            .map(|table| {
                serde_json::json!({
                    "schema": table.schema.clone(),
                    "name": table.name.clone(),
                    "full_name": format!("{}.{}", table.schema, table.name)
                })
            })
            .collect::<Vec<_>>();
        let meta = build_meta(
            data.len(),
            &state.config.planner_mode,
            None,
            &Vec::new(),
        );
        return (
            StatusCode::OK,
            Json(QueryResponse {
                sql: "".to_string(),
                params: vec![],
                data,
                meta,
            }),
        )
            .into_response();
    }
    if plan.operation == Operation::DescribeTable {
        let table_info = match db_qa_core::index::resolve_table(&index, &plan.from.table) {
            Ok(table) => table,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        };
        let data = vec![serde_json::json!({
            "schema": table_info.schema.clone(),
            "name": table_info.name.clone(),
            "full_name": format!("{}.{}", table_info.schema, table_info.name),
            "columns": table_info.columns.clone(),
            "primary_keys": table_info.primary_keys.clone()
        })];
        let meta = build_meta(
            data.len(),
            &state.config.planner_mode,
            None,
            &Vec::new(),
        );
        return (
            StatusCode::OK,
            Json(QueryResponse {
                sql: "".to_string(),
                params: vec![],
                data,
                meta,
            }),
        )
            .into_response();
    }

    let compiled = match compile_plan(&plan, &index, &state.config) {
        Ok(compiled) => compiled,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };

    let mut tx = match state.pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };
    if let Err(err) = sqlx::query("SET LOCAL statement_timeout = $1")
        .bind(state.config.statement_timeout_ms)
        .execute(&mut *tx)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response();
    }

    let mut query = sqlx::query(&compiled.sql);
    for param in &compiled.params {
        query = bind_param(query, param);
    }

    let rows = match query.fetch_all(&mut *tx).await {
        Ok(rows) => rows,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };

    let data = match rows_to_json(rows) {
        Ok(data) => data,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response();
        }
    };

    let params_json = compiled.params.iter().map(param_to_json).collect();
    let meta = build_meta(
        data.len(),
        &state.config.planner_mode,
        Some(compiled.applied_limit),
        &compiled.warnings,
    );

    (
        StatusCode::OK,
        Json(QueryResponse {
            sql: compiled.sql,
            params: params_json,
            data,
            meta,
        }),
    )
        .into_response()
}

fn bind_param<'q>(mut query: sqlx::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, param: &ParamValue) -> sqlx::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match param {
        ParamValue::String(value) => query = query.bind(value),
        ParamValue::Int(value) => query = query.bind(value),
        ParamValue::Float(value) => query = query.bind(value),
        ParamValue::Bool(value) => query = query.bind(value),
    }
    query
}

fn rows_to_json(rows: Vec<PgRow>) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut data = Vec::new();
    for row in rows {
        let mut map = serde_json::Map::new();
        for (idx, column) in row.columns().iter().enumerate() {
            let value: serde_json::Value = row.try_get(idx)?;
            map.insert(column.name().to_string(), value);
        }
        data.push(serde_json::Value::Object(map));
    }
    Ok(data)
}

fn param_to_json(param: &ParamValue) -> serde_json::Value {
    match param {
        ParamValue::String(value) => serde_json::Value::String(value.clone()),
        ParamValue::Int(value) => serde_json::Value::Number((*value).into()),
        ParamValue::Float(value) => serde_json::Value::Number(
            serde_json::Number::from_f64(*value).unwrap_or_else(|| serde_json::Number::from(0)),
        ),
        ParamValue::Bool(value) => serde_json::Value::Bool(*value),
    }
}

fn build_meta(
    row_count: usize,
    planner_mode: &PlannerMode,
    limit: Option<i64>,
    warnings: &Vec<String>,
) -> serde_json::Value {
    serde_json::json!({
        "row_count": row_count,
        "planner_mode": match planner_mode { PlannerMode::Rules => "rules", PlannerMode::OpenAi => "openai" },
        "limit": limit,
        "warnings": warnings
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_limit_uses_applied_value() {
        let meta = build_meta(1, &PlannerMode::Rules, Some(42), &Vec::new());
        assert_eq!(meta.get("limit"), Some(&serde_json::json!(42)));
    }
}
