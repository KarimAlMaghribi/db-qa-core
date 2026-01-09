use crate::config::{Config, PlannerMode};
use crate::index::{resolve_table, SchemaIndex};
use crate::ir::{FilterItem, FromClause, Operation, OrderByItem, QueryPlan, SelectItem};
use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::Value;

pub async fn plan_query(prompt: &str, index: &SchemaIndex, config: &Config) -> Result<QueryPlan> {
    match config.planner_mode {
        PlannerMode::Rules => rules_plan(prompt, index, config),
        PlannerMode::OpenAi => openai_plan(prompt, index, config).await,
    }
}

fn rules_plan(prompt: &str, index: &SchemaIndex, _config: &Config) -> Result<QueryPlan> {
    let normalized = prompt.trim();
    if normalized.eq_ignore_ascii_case("list tables") {
        return Ok(QueryPlan {
            operation: Operation::ListTables,
            from: FromClause { table: "".to_string() },
            select: Vec::new(),
            filters: Vec::new(),
            order_by: Vec::new(),
            limit: None,
        });
    }
    let describe_re = Regex::new(r"(?i)^describe\s+table\s+(?P<table>.+)$")?;
    if let Some(captures) = describe_re.captures(normalized) {
        let table_name = captures.name("table").map(|m| m.as_str().trim()).unwrap_or("");
        if table_name.is_empty() {
            return Err(anyhow!("describe table requires a table name"));
        }
        let table_info = resolve_table(index, table_name)?;
        return Ok(QueryPlan {
            operation: Operation::DescribeTable,
            from: FromClause {
                table: format!("{}.{}", table_info.schema, table_info.name),
            },
            select: Vec::new(),
            filters: Vec::new(),
            order_by: Vec::new(),
            limit: None,
        });
    }
    let re = Regex::new(
        r"(?ix)^select\s+(?P<select>[^;]+?)\s+from\s+(?P<table>[a-z0-9_.]+)(?:\s+where\s+(?P<where>[^;]+?))?(?:\s+order\s+by\s+(?P<order>[^;]+?))?(?:\s+limit\s+(?P<limit>\d+))?$",
    )?;
    let captures = re
        .captures(normalized)
        .ok_or_else(|| anyhow!("prompt does not match rules grammar"))?;

    let table = captures.name("table").unwrap().as_str();
    let table_info = resolve_table(index, table)?;

    let select_raw = captures.name("select").unwrap().as_str();
    let select = parse_select_list(select_raw, table_info)?;

    let filters = captures
        .name("where")
        .map(|value| parse_where(value.as_str(), table_info))
        .transpose()?
        .unwrap_or_default();

    let order_by = captures
        .name("order")
        .map(|value| parse_order(value.as_str(), table_info))
        .transpose()?
        .unwrap_or_default();

    let limit = captures
        .name("limit")
        .and_then(|value| value.as_str().parse::<i64>().ok());

    Ok(QueryPlan {
        operation: Operation::Select,
        from: FromClause {
            table: format!("{}.{}", table_info.schema, table_info.name),
        },
        select,
        filters,
        order_by,
        limit,
    })
}

fn parse_select_list(raw: &str, table: &crate::index::TableInfo) -> Result<Vec<SelectItem>> {
    if raw.trim() == "*" {
        return Ok(table
            .columns
            .iter()
            .map(|column| SelectItem {
                column: format!("{}.{}.{}", table.schema, table.name, column),
                r#as: None,
            })
            .collect());
    }
    let mut items = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut split = trimmed.split_whitespace();
        let column = split.next().unwrap();
        let alias = split.next().map(|value| value.to_string());
        items.push(SelectItem {
            column: qualify_column(column, table)?,
            r#as: alias,
        });
    }
    if items.is_empty() {
        return Err(anyhow!("empty select list"));
    }
    Ok(items)
}

fn parse_where(raw: &str, table: &crate::index::TableInfo) -> Result<Vec<FilterItem>> {
    let mut filters = Vec::new();
    let splitter = Regex::new("(?i)\\s+and\\s+")?;
    for part in splitter.split(raw) {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let pieces: Vec<&str> = trimmed.split('=').collect();
        if pieces.len() != 2 {
            return Err(anyhow!("invalid where clause"));
        }
        let column = pieces[0].trim();
        let value_raw = pieces[1].trim().trim_matches('"').trim_matches('\'');
        let value = parse_scalar(value_raw);
        filters.push(FilterItem {
            column: qualify_column(column, table)?,
            op: "=".to_string(),
            value,
        });
    }
    Ok(filters)
}

fn parse_order(raw: &str, table: &crate::index::TableInfo) -> Result<Vec<OrderByItem>> {
    let mut items = Vec::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut split = trimmed.split_whitespace();
        let column = split.next().unwrap();
        let dir = split.next().unwrap_or("asc");
        items.push(OrderByItem {
            column: qualify_column(column, table)?,
            dir: dir.to_string(),
        });
    }
    Ok(items)
}

fn qualify_column(column: &str, table: &crate::index::TableInfo) -> Result<String> {
    let parts: Vec<&str> = column.split('.').collect();
    match parts.len() {
        1 => Ok(format!("{}.{}.{}", table.schema, table.name, parts[0])),
        2 => Ok(format!("{}.{}.{}", table.schema, parts[0], parts[1])),
        3 => Ok(format!("{}.{}.{}", parts[0], parts[1], parts[2])),
        _ => Err(anyhow!("invalid column reference")),
    }
}

fn parse_scalar(raw: &str) -> Value {
    if let Ok(int) = raw.parse::<i64>() {
        return Value::from(int);
    }
    if let Ok(float) = raw.parse::<f64>() {
        return Value::from(float);
    }
    if raw.eq_ignore_ascii_case("true") {
        return Value::from(true);
    }
    if raw.eq_ignore_ascii_case("false") {
        return Value::from(false);
    }
    Value::from(raw.to_string())
}

async fn openai_plan(prompt: &str, index: &SchemaIndex, config: &Config) -> Result<QueryPlan> {
    let Some(key) = config.openai_key.as_deref() else {
        return Err(anyhow!("OPENAI_API_KEY is not set"));
    };
    let schema_summary = index
        .tables
        .values()
        .map(|table| {
            format!(
                "{}.{}, columns: {}",
                table.schema,
                table.name,
                table.columns.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = format!(
        "You are a SQL planner. Return JSON only. Schema:\n{}\nReturn JSON matching the IR schema.",
        schema_summary
    );

    let response = reqwest::Client::new()
        .post("https://api.openai.com/v1/chat/completions")
        .bearer_auth(key)
        .json(&serde_json::json!({
            "model": config.openai_model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": prompt}
            ],
            "temperature": 0
        }))
        .send()
        .await?
        .error_for_status()?;

    let body: serde_json::Value = response.json().await?;
    let content = body
        .pointer("/choices/0/message/content")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("invalid openai response"))?;
    let plan: QueryPlan = serde_json::from_str(content)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, PlannerMode};
    use crate::index::SchemaIndex;
    use std::collections::HashMap;

    fn sample_index() -> SchemaIndex {
        let mut tables = HashMap::new();
        tables.insert(
            "public.users".to_string(),
            crate::index::TableInfo {
                schema: "public".to_string(),
                name: "users".to_string(),
                columns: vec!["id".to_string(), "email".to_string(), "name".to_string()],
                primary_keys: vec!["id".to_string()],
            },
        );
        tables.insert(
            "public.orders".to_string(),
            crate::index::TableInfo {
                schema: "public".to_string(),
                name: "orders".to_string(),
                columns: vec!["id".to_string(), "status".to_string()],
                primary_keys: vec!["id".to_string()],
            },
        );
        SchemaIndex {
            tables,
            fingerprint: "fp".to_string(),
        }
    }

    fn rules_config() -> Config {
        Config {
            pg_url: "".to_string(),
            sqlite_path: "".to_string(),
            host: "".to_string(),
            port: 0,
            default_limit: 100,
            max_limit: 1000,
            statement_timeout_ms: 2000,
            planner_mode: PlannerMode::Rules,
            openai_key: None,
            openai_model: "".to_string(),
        }
    }

    #[test]
    fn list_tables_parsing() {
        let index = sample_index();
        let plan = rules_plan("list tables", &index, &rules_config()).unwrap();
        assert_eq!(plan.operation, Operation::ListTables);
    }

    #[test]
    fn describe_table_parsing() {
        let index = sample_index();
        let plan = rules_plan("Describe Table users", &index, &rules_config()).unwrap();
        assert_eq!(plan.operation, Operation::DescribeTable);
        assert_eq!(plan.from.table, "public.users");
    }

    #[test]
    fn select_star_expands_in_order() {
        let index = sample_index();
        let plan = rules_plan("select * from users", &index, &rules_config()).unwrap();
        let columns = plan
            .select
            .iter()
            .map(|item| item.column.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            columns,
            vec![
                "public.users.id".to_string(),
                "public.users.email".to_string(),
                "public.users.name".to_string()
            ]
        );
    }

    #[test]
    fn and_is_case_insensitive() {
        let index = sample_index();
        let plan = rules_plan(
            "select id from users where id=1 AND email=alice@example.com",
            &index,
            &rules_config(),
        )
        .unwrap();
        assert_eq!(plan.filters.len(), 2);
    }
}
