# db-qa-core MVP

A deterministic, local-first SQL QA service for Postgres. It introspects schema, builds a local schema index (SQLite), and answers prompts by compiling a rules-based query plan into safe, parameterized SELECT SQL.

## Requirements
- Docker + Docker Compose
- Rust (stable)

## Quick start

```bash
# 1) Start Postgres with sample data
docker compose up -d

# 2) Run the service (defaults to localhost:8080)
export PG_URL="postgres://postgres:postgres@localhost:5432/dbqa"
export PLANNER_MODE=rules
cargo run
```

### Health + readiness

```bash
curl -s http://localhost:8080/health
curl -s http://localhost:8080/ready
```

### Query examples (rules planner)

```bash
curl -s -X POST http://localhost:8080/query \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"select id, email from users where id=1 limit 5"}' | jq
```

```bash
curl -s -X POST http://localhost:8080/query \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"select id, status, total_cents from orders where status=paid order by id desc limit 2"}' | jq
```

```bash
curl -s -X POST http://localhost:8080/query \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"list tables"}' | jq
```

```bash
curl -s -X POST http://localhost:8080/query \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"describe table users"}' | jq
```

```bash
curl -s -X POST http://localhost:8080/query \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"select * from users limit 2"}' | jq
```

### Rebuild the schema index

```bash
curl -s -X POST http://localhost:8080/index/rebuild | jq
```

### Index status

```bash
curl -s http://localhost:8080/index/status | jq
```

## Planner modes

- `rules` (default): deterministic grammar parsing.
- `openai`: optional; only active when `OPENAI_API_KEY` is set.

```bash
export PLANNER_MODE=openai
export OPENAI_API_KEY=... # optional
```

## Configuration

| Env var | Default | Description |
| --- | --- | --- |
| `PG_URL` | `postgres://postgres:postgres@localhost:5432/dbqa` | Postgres connection string |
| `INDEX_DB_PATH` | `./data/index.db` | SQLite path for schema index |
| `HOST` | `0.0.0.0` | Bind host |
| `PORT` | `8080` | Bind port |
| `DEFAULT_LIMIT` | `100` | Default row limit if none provided |
| `MAX_LIMIT` | `1000` | Hard cap for row limit |
| `STATEMENT_TIMEOUT_MS` | `2000` | Per-query timeout |
| `PLANNER_MODE` | `rules` | Planner mode (`rules` or `openai`) |
| `OPENAI_API_KEY` | empty | OpenAI API key for optional planner |
| `OPENAI_MODEL` | `gpt-4o-mini` | OpenAI model |

## Tests

```bash
cargo test
```

## What was created
- `Cargo.toml`, `src/` Rust service
- `docker-compose.yml` + `migrations/seed.sql`
- `README.md`

## How to run
1. `docker compose up -d`
2. `cargo run`
3. `curl -X POST http://localhost:8080/query ...`
