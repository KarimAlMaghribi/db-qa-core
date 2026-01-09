use crate::config::Config;
use crate::index::{resolve_table, SchemaIndex};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Select,
    Aggregate,
    ListTables,
    DescribeTable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FromClause {
    pub table: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SelectItem {
    pub column: String,
    #[serde(default)]
    pub r#as: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilterItem {
    pub column: String,
    pub op: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderByItem {
    pub column: String,
    pub dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryPlan {
    pub operation: Operation,
    pub from: FromClause,
    pub select: Vec<SelectItem>,
    #[serde(default)]
    pub filters: Vec<FilterItem>,
    #[serde(default)]
    pub order_by: Vec<OrderByItem>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub sql: String,
    pub params: Vec<ParamValue>,
    pub applied_limit: i64,
    pub warnings: Vec<String>,
}

pub fn validate_plan(plan: &QueryPlan) -> Result<()> {
    if plan.operation != Operation::Select {
        return Err(anyhow!("only select operation supported"));
    }
    if plan.select.is_empty() {
        return Err(anyhow!("select list cannot be empty"));
    }
    Ok(())
}

pub fn compile_plan(plan: &QueryPlan, index: &SchemaIndex, config: &Config) -> Result<CompiledQuery> {
    validate_plan(plan)?;
    let table_info = resolve_table(index, &plan.from.table)?;
    let mut params = Vec::new();
    let mut warnings = Vec::new();
    let mut alias_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    let select_items = plan
        .select
        .iter()
        .map(|item| {
            let (schema, table, column) = split_column(&item.column, table_info)?;
            if schema != table_info.schema || table != table_info.name {
                return Err(anyhow!("select columns across tables not supported"));
            }
            ensure_column_exists(table_info, &column)?;
            let base_alias = item.r#as.clone().unwrap_or_else(|| column.clone());
            let alias = match alias_counts.get_mut(&base_alias) {
                Some(count) => {
                    *count += 1;
                    let renamed = format!("{}_{}", base_alias, count);
                    warnings.push(format!(
                        "alias '{}' duplicated, renamed to '{}'",
                        base_alias, renamed
                    ));
                    renamed
                }
                None => {
                    alias_counts.insert(base_alias.clone(), 1);
                    base_alias
                }
            };
            Ok(format!(
                "to_jsonb({}.{}.{}) AS {}",
                quote_ident(&schema),
                quote_ident(&table),
                quote_ident(&column),
                quote_ident(&alias)
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut sql = format!(
        "SELECT {} FROM {}.{}",
        select_items.join(", "),
        quote_ident(&table_info.schema),
        quote_ident(&table_info.name)
    );

    if !plan.filters.is_empty() {
        let mut clauses = Vec::new();
        for filter in &plan.filters {
            if filter.op != "=" {
                return Err(anyhow!("unsupported filter op: {}", filter.op));
            }
            let (schema, table, column) = split_column(&filter.column, table_info)?;
            if schema != table_info.schema || table != table_info.name {
                return Err(anyhow!("filters across tables not supported"));
            }
            ensure_column_exists(table_info, &column)?;
            let placeholder = format!("${}", params.len() + 1);
            clauses.push(format!(
                "{}.{}.{} = {}",
                quote_ident(&schema),
                quote_ident(&table),
                quote_ident(&column),
                placeholder
            ));
            params.push(parse_param_value(&filter.value)?);
        }
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    let order_by = if plan.order_by.is_empty() {
        default_order_by(table_info)?
    } else {
        plan.order_by.clone()
    };

    if !order_by.is_empty() {
        let mut clauses = Vec::new();
        for item in order_by {
            let (schema, table, column) = split_column(&item.column, table_info)?;
            if schema != table_info.schema || table != table_info.name {
                return Err(anyhow!("order by across tables not supported"));
            }
            ensure_column_exists(table_info, &column)?;
            let dir = match item.dir.to_lowercase().as_str() {
                "asc" => "ASC",
                "desc" => "DESC",
                _ => return Err(anyhow!("invalid order direction")),
            };
            clauses.push(format!(
                "{}.{}.{} {}",
                quote_ident(&schema),
                quote_ident(&table),
                quote_ident(&column),
                dir
            ));
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(&clauses.join(", "));
    }

    let mut limit = plan.limit.unwrap_or(config.default_limit);
    if limit <= 0 {
        limit = config.default_limit;
    }
    if limit > config.max_limit {
        limit = config.max_limit;
    }
    sql.push_str(&format!(" LIMIT {}", limit));

    Ok(CompiledQuery {
        sql,
        params,
        applied_limit: limit,
        warnings,
    })
}

fn parse_param_value(value: &serde_json::Value) -> Result<ParamValue> {
    match value {
        serde_json::Value::String(text) => Ok(ParamValue::String(text.clone())),
        serde_json::Value::Number(num) => {
            if let Some(int) = num.as_i64() {
                Ok(ParamValue::Int(int))
            } else if let Some(float) = num.as_f64() {
                Ok(ParamValue::Float(float))
            } else {
                Err(anyhow!("unsupported number"))
            }
        }
        serde_json::Value::Bool(value) => Ok(ParamValue::Bool(*value)),
        _ => Err(anyhow!("unsupported param type")),
    }
}

fn default_order_by(table: &crate::index::TableInfo) -> Result<Vec<OrderByItem>> {
    if table.primary_keys.is_empty() {
        return Ok(Vec::new());
    }
    Ok(table
        .primary_keys
        .iter()
        .map(|pk| OrderByItem {
            column: format!("{}.{}.{}", table.schema, table.name, pk),
            dir: "asc".to_string(),
        })
        .collect())
}

fn ensure_column_exists(table: &crate::index::TableInfo, column: &str) -> Result<()> {
    if table.columns.iter().any(|item| item == column) {
        Ok(())
    } else {
        Err(anyhow!("unknown column: {column}"))
    }
}

fn split_column(column: &str, table: &crate::index::TableInfo) -> Result<(String, String, String)> {
    let parts: Vec<&str> = column.split('.').collect();
    match parts.len() {
        1 => Ok((table.schema.clone(), table.name.clone(), parts[0].to_string())),
        2 => Ok((table.schema.clone(), parts[0].to_string(), parts[1].to_string())),
        3 => Ok((parts[0].to_string(), parts[1].to_string(), parts[2].to_string())),
        _ => Err(anyhow!("invalid column reference: {column}")),
    }
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SchemaIndex;
    use std::collections::HashMap;

    fn sample_index() -> SchemaIndex {
        let mut tables = HashMap::new();
        tables.insert(
            "public.users".to_string(),
            crate::index::TableInfo {
                schema: "public".to_string(),
                name: "users".to_string(),
                columns: vec!["id".to_string(), "email".to_string()],
                primary_keys: vec!["id".to_string()],
            },
        );
        SchemaIndex {
            tables,
            fingerprint: "fp".to_string(),
        }
    }

    #[test]
    fn compile_adds_defaults_and_params() {
        let index = sample_index();
        let plan = QueryPlan {
            operation: Operation::Select,
            from: FromClause {
                table: "users".to_string(),
            },
            select: vec![SelectItem {
                column: "email".to_string(),
                r#as: None,
            }],
            filters: vec![FilterItem {
                column: "id".to_string(),
                op: "=".to_string(),
                value: serde_json::json!(1),
            }],
            order_by: vec![],
            limit: None,
        };
        let config = Config {
            pg_url: "".to_string(),
            sqlite_path: "".to_string(),
            host: "".to_string(),
            port: 0,
            default_limit: 100,
            max_limit: 1000,
            statement_timeout_ms: 2000,
            planner_mode: crate::config::PlannerMode::Rules,
            openai_key: None,
            openai_model: "".to_string(),
        };
        let compiled = compile_plan(&plan, &index, &config).unwrap();
        assert!(compiled.sql.contains("ORDER BY \"public\".\"users\".\"id\" ASC"));
        assert!(compiled.sql.contains("LIMIT 100"));
        assert_eq!(compiled.params, vec![ParamValue::Int(1)]);
        assert_eq!(compiled.applied_limit, 100);
    }

    #[test]
    fn validate_requires_select_list() {
        let plan = QueryPlan {
            operation: Operation::Select,
            from: FromClause {
                table: "users".to_string(),
            },
            select: vec![],
            filters: vec![],
            order_by: vec![],
            limit: None,
        };
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn compiler_blocks_cross_table_columns() {
        let mut tables = HashMap::new();
        tables.insert(
            "public.users".to_string(),
            crate::index::TableInfo {
                schema: "public".to_string(),
                name: "users".to_string(),
                columns: vec!["id".to_string(), "email".to_string()],
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
        let index = SchemaIndex {
            tables,
            fingerprint: "fp".to_string(),
        };
        let plan = QueryPlan {
            operation: Operation::Select,
            from: FromClause {
                table: "users".to_string(),
            },
            select: vec![SelectItem {
                column: "orders.id".to_string(),
                r#as: None,
            }],
            filters: vec![],
            order_by: vec![],
            limit: None,
        };
        let config = Config {
            pg_url: "".to_string(),
            sqlite_path: "".to_string(),
            host: "".to_string(),
            port: 0,
            default_limit: 100,
            max_limit: 1000,
            statement_timeout_ms: 2000,
            planner_mode: crate::config::PlannerMode::Rules,
            openai_key: None,
            openai_model: "".to_string(),
        };
        let err = compile_plan(&plan, &index, &config).unwrap_err();
        assert!(err.to_string().contains("select columns across tables"));
    }

    #[test]
    fn compiler_applies_limit_clamp() {
        let index = sample_index();
        let plan = QueryPlan {
            operation: Operation::Select,
            from: FromClause {
                table: "users".to_string(),
            },
            select: vec![SelectItem {
                column: "email".to_string(),
                r#as: None,
            }],
            filters: vec![],
            order_by: vec![],
            limit: Some(50),
        };
        let config = Config {
            pg_url: "".to_string(),
            sqlite_path: "".to_string(),
            host: "".to_string(),
            port: 0,
            default_limit: 5,
            max_limit: 10,
            statement_timeout_ms: 2000,
            planner_mode: crate::config::PlannerMode::Rules,
            openai_key: None,
            openai_model: "".to_string(),
        };
        let compiled = compile_plan(&plan, &index, &config).unwrap();
        assert_eq!(compiled.applied_limit, 10);
    }

    #[test]
    fn duplicate_aliases_get_renamed_with_warning() {
        let index = sample_index();
        let plan = QueryPlan {
            operation: Operation::Select,
            from: FromClause {
                table: "users".to_string(),
            },
            select: vec![
                SelectItem {
                    column: "email".to_string(),
                    r#as: Some("dup".to_string()),
                },
                SelectItem {
                    column: "id".to_string(),
                    r#as: Some("dup".to_string()),
                },
            ],
            filters: vec![],
            order_by: vec![],
            limit: None,
        };
        let config = Config {
            pg_url: "".to_string(),
            sqlite_path: "".to_string(),
            host: "".to_string(),
            port: 0,
            default_limit: 100,
            max_limit: 1000,
            statement_timeout_ms: 2000,
            planner_mode: crate::config::PlannerMode::Rules,
            openai_key: None,
            openai_model: "".to_string(),
        };
        let compiled = compile_plan(&plan, &index, &config).unwrap();
        assert!(compiled.sql.contains("AS \"dup\""));
        assert!(compiled.sql.contains("AS \"dup_2\""));
        assert_eq!(compiled.warnings.len(), 1);
    }
}
