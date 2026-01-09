use db_qa_core::config::Config;
use db_qa_core::index::ensure_fresh_index;
use db_qa_core::ir::{compile_plan, FromClause, Operation, QueryPlan, SelectItem};

#[tokio::test]
#[ignore]
async fn integration_query_builds() {
    let config = Config::from_env();
    let pool = sqlx::PgPool::connect(&config.pg_url).await.unwrap();
    let index = ensure_fresh_index(&config.sqlite_path, &pool).await.unwrap();

    let plan = QueryPlan {
        operation: Operation::Select,
        from: FromClause {
            table: "users".to_string(),
        },
        select: vec![SelectItem {
            column: "id".to_string(),
            r#as: None,
        }],
        filters: vec![],
        order_by: vec![],
        limit: Some(1),
    };

    let compiled = compile_plan(&plan, &index, &config).unwrap();
    let rows = sqlx::query(&compiled.sql)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(!rows.is_empty());
}
