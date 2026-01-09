use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const SCHEMA_VERSION: &str = "1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaIndex {
    pub tables: HashMap<String, TableInfo>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub columns: Vec<String>,
    pub primary_keys: Vec<String>,
}

impl SchemaIndex {
    pub fn empty() -> Self {
        Self {
            tables: HashMap::new(),
            fingerprint: "".to_string(),
        }
    }
}

pub async fn build_schema_index(pool: &PgPool) -> Result<SchemaIndex> {
    let tables = sqlx::query!(
        r#"
        SELECT table_schema, table_name
        FROM information_schema.tables
        WHERE table_type = 'BASE TABLE'
          AND table_schema NOT IN ('pg_catalog', 'information_schema')
        ORDER BY table_schema, table_name
        "#
    )
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::new();

    for table in tables {
        let columns = sqlx::query!(
            r#"
            SELECT column_name
            FROM information_schema.columns
            WHERE table_schema = $1 AND table_name = $2
            ORDER BY ordinal_position
            "#,
            table.table_schema,
            table.table_name
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| row.column_name)
        .collect::<Vec<_>>();

        let primary_keys = sqlx::query!(
            r#"
            SELECT a.attname as column_name
            FROM pg_index i
            JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
            JOIN pg_class c ON c.oid = i.indrelid
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE i.indisprimary = true
              AND n.nspname = $1
              AND c.relname = $2
            ORDER BY array_position(i.indkey, a.attnum)
            "#,
            table.table_schema,
            table.table_name
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| row.column_name)
        .collect::<Vec<_>>();

        let key = format!("{}.{}", table.table_schema, table.table_name);
        map.insert(
            key,
            TableInfo {
                schema: table.table_schema,
                name: table.table_name,
                columns,
                primary_keys,
            },
        );
    }

    let fingerprint = compute_fingerprint(&map);

    Ok(SchemaIndex { tables: map, fingerprint })
}

fn compute_fingerprint(tables: &HashMap<String, TableInfo>) -> String {
    let mut entries = tables.values().collect::<Vec<_>>();
    entries.sort_by(|a, b| (a.schema.clone(), a.name.clone()).cmp(&(b.schema.clone(), b.name.clone())));
    let mut hasher = Sha256::new();
    for table in entries {
        hasher.update(table.schema.as_bytes());
        hasher.update(table.name.as_bytes());
        for column in &table.columns {
            hasher.update(column.as_bytes());
        }
        for pk in &table.primary_keys {
            hasher.update(pk.as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

pub fn load_index(sqlite_path: &str) -> Result<Option<SchemaIndex>> {
    if !Path::new(sqlite_path).exists() {
        return Ok(None);
    }
    let conn = Connection::open(sqlite_path)?;
    ensure_sqlite_schema(&conn)?;
    let fingerprint: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'fingerprint'", [], |row| row.get(0))
        .optional()?;
    let Some(fingerprint) = fingerprint else {
        return Ok(None);
    };

    let mut tables = HashMap::new();
    let mut stmt = conn.prepare("SELECT schema, name, primary_keys FROM tables")?;
    let table_rows = stmt.query_map([], |row| {
        let schema: String = row.get(0)?;
        let name: String = row.get(1)?;
        let primary_keys_json: String = row.get(2)?;
        let primary_keys: Vec<String> = serde_json::from_str(&primary_keys_json).unwrap_or_default();
        Ok((schema, name, primary_keys))
    })?;

    for row in table_rows {
        let (schema, name, primary_keys) = row?;
        let key = format!("{}.{}", schema, name);
        let mut column_stmt = conn.prepare(
            "SELECT name FROM columns WHERE schema = ?1 AND table_name = ?2 ORDER BY position",
        )?;
        let column_rows = column_stmt.query_map(params![schema, name], |row| row.get(0))?;
        let mut columns = Vec::new();
        for column in column_rows {
            columns.push(column?);
        }
        tables.insert(
            key,
            TableInfo {
                schema,
                name,
                columns,
                primary_keys,
            },
        );
    }

    Ok(Some(SchemaIndex { tables, fingerprint }))
}

pub fn load_meta(sqlite_path: &str) -> Result<HashMap<String, String>> {
    if !Path::new(sqlite_path).exists() {
        return Ok(HashMap::new());
    }
    let conn = Connection::open(sqlite_path)?;
    ensure_sqlite_schema(&conn)?;
    let mut meta = HashMap::new();
    let mut stmt = conn.prepare("SELECT key, value FROM meta")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (key, value) = row?;
        meta.insert(key, value);
    }
    Ok(meta)
}

pub fn save_index(sqlite_path: &str, index: &SchemaIndex) -> Result<()> {
    if let Some(parent) = Path::new(sqlite_path).parent() {
        fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(sqlite_path)?;
    ensure_sqlite_schema(&conn)?;
    conn.execute("DELETE FROM meta", [])?;
    conn.execute("DELETE FROM tables", [])?;
    conn.execute("DELETE FROM columns", [])?;
    let last_built_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('fingerprint', ?1)",
        params![index.fingerprint],
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('last_built_at', ?1)",
        params![last_built_at],
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![SCHEMA_VERSION],
    )?;

    for table in index.tables.values() {
        let primary_keys = serde_json::to_string(&table.primary_keys)?;
        conn.execute(
            "INSERT INTO tables (schema, name, primary_keys) VALUES (?1, ?2, ?3)",
            params![table.schema, table.name, primary_keys],
        )?;
        for (position, column) in table.columns.iter().enumerate() {
            conn.execute(
                "INSERT INTO columns (schema, table_name, name, position) VALUES (?1, ?2, ?3, ?4)",
                params![table.schema, table.name, column, position as i64],
            )?;
        }
    }
    Ok(())
}

pub async fn ensure_fresh_index(
    sqlite_path: &str,
    pool: &PgPool,
) -> Result<SchemaIndex> {
    let index = build_schema_index(pool).await?;
    let existing = load_index(sqlite_path)?;
    if let Some(existing) = existing {
        if existing.fingerprint == index.fingerprint {
            return Ok(existing);
        }
    }
    save_index(sqlite_path, &index)?;
    Ok(index)
}

pub async fn rebuild_index(sqlite_path: &str, pool: &PgPool) -> Result<SchemaIndex> {
    let index = build_schema_index(pool).await?;
    save_index(sqlite_path, &index)?;
    Ok(index)
}

fn ensure_sqlite_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS tables (
            schema TEXT NOT NULL,
            name TEXT NOT NULL,
            primary_keys TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS columns (
            schema TEXT NOT NULL,
            table_name TEXT NOT NULL,
            name TEXT NOT NULL,
            position INTEGER NOT NULL
        );
        "#,
    )
    .context("create sqlite schema")?;
    Ok(())
}

pub fn resolve_table(index: &SchemaIndex, table: &str) -> Result<&TableInfo> {
    if let Some(info) = index.tables.get(table) {
        return Ok(info);
    }
    let matches: Vec<&TableInfo> = index
        .tables
        .values()
        .filter(|info| info.name == table)
        .collect();
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    Err(anyhow!("unknown table: {table}"))
}
