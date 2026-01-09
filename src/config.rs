use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub pg_url: String,
    pub sqlite_path: String,
    pub host: String,
    pub port: u16,
    pub default_limit: i64,
    pub max_limit: i64,
    pub statement_timeout_ms: i64,
    pub planner_mode: PlannerMode,
    pub openai_key: Option<String>,
    pub openai_model: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannerMode {
    Rules,
    OpenAi,
}

impl Config {
    pub fn from_env() -> Self {
        let pg_url = env::var("PG_URL")
            .or_else(|_| env::var("DATABASE_URL"))
            .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/dbqa".to_string());
        let sqlite_path = env::var("INDEX_DB_PATH").unwrap_or_else(|_| "./data/index.db".to_string());
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let port = env::var("PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8080);
        let default_limit = env::var("DEFAULT_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(100);
        let max_limit = env::var("MAX_LIMIT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1000);
        let statement_timeout_ms = env::var("STATEMENT_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(2000);
        let planner_mode = match env::var("PLANNER_MODE").as_deref() {
            Ok("openai") => PlannerMode::OpenAi,
            _ => PlannerMode::Rules,
        };
        let openai_key = env::var("OPENAI_API_KEY").ok();
        let openai_model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

        Self {
            pg_url,
            sqlite_path,
            host,
            port,
            default_limit,
            max_limit,
            statement_timeout_ms,
            planner_mode,
            openai_key,
            openai_model,
        }
    }
}
