# db-qa-core — MVP Spec (local runnable)

Goal:
Build a generic, fast, stable service that connects to a SQL database (start with Postgres),
auto-introspects schema, builds a local schema index ("RAG over schema"), and answers prompts by
compiling a deterministic query-plan (IR) into safe SELECT SQL, returning strict JSON.

Non-negotiables:
- Single external data dependency: the SQL database.
- After initial indexing, queries must work automatically (no manual mapping).
- Deterministic outputs: same prompt + same DB state => same JSON structure and values.
- Runtime must be fast: retrieval + planning + SQL execution should be optimized and cached.
- Security: service executes SELECT-only; parameterized SQL; timeouts; row limits.

Tech:
- Rust (axum + tokio)
- Postgres via sqlx (async pool)
- Local persistent storage for catalog + index: SQLite or RocksDB (choose one)
- Local schema retrieval:
  - MVP: lexical + optional embeddings (pluggable)
  - Provide config to enable embeddings later (do not block MVP on model downloads).
- Planner modes:
  1) rules (no LLM): supports a small grammar to prove E2E behavior locally
  2) openai (optional): if OPENAI_API_KEY provided, call LLM to output IR JSON
- Formatter must be code-based (no LLM required for JSON formatting).

Service behavior:
- On startup: connect to DB, compute schema fingerprint; if missing/outdated index => build index; then READY.
- Endpoints:
  - GET /health -> 200
  - GET /ready  -> 200 if indexed, else 503 with status json
  - POST /index/rebuild -> triggers rebuild
  - POST /query -> { "prompt": "...", "top_k": 12 } -> strict JSON response containing:
      { "sql": "...", "params": [...], "data": [...], "meta": {...} }
- IR (query-plan) JSON schema (minimal for MVP):
  - operation: "select" | "aggregate"
  - from: { table: string }
  - select: [{ column: "schema.table.column" , as: "alias" }]
  - filters: [{ column: "schema.table.column", op: "=", value: <scalar> }]
  - order_by: [{ column: "...", dir: "asc"|"desc" }]
  - limit: number
- Deterministic resolver rules:
  - Always choose shortest FK join path; tie-break lexicographically.
  - Always apply ORDER BY primary key asc if not provided.
  - Always apply a default LIMIT if not provided (e.g. 100).
- Compiler:
  - Generate SELECT-only SQL, parameterized, with identifier quoting.
  - Reject non-existing columns/tables.
  - Hard caps: max joins, max rows, statement timeout.

Local runnable:
- Provide docker-compose.yml with Postgres + sample schema + seed data.
- Provide README with:
  - docker compose up
  - cargo run
  - curl examples for /query
- Provide tests:
  - Unit test for IR validation and deterministic compilation
  - Integration test (can be optional/ignored by default) or a script that runs a query against local Postgres

Definition of Done:
- `docker compose up -d` starts Postgres with sample data.
- `cargo run` starts the service and reaches READY.
- `curl -X POST /query` returns strict JSON with correct DB values.
- `cargo test` passes.
