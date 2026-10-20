use std::collections::HashSet;

use sea_orm::{Statement, TransactionTrait};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let txn = manager.get_connection().begin().await?;
        txn.execute_unprepared("ALTER TABLE library ADD COLUMN name TEXT NOT NULL DEFAULT ''")
            .await?;
        let backend = txn.get_database_backend();
        let rows = txn
            .query_all(Statement::from_string(
                backend,
                "SELECT id, path FROM library ORDER BY id",
            ))
            .await?;
        let mut used = HashSet::new();
        for row in rows {
            let id: i64 = row.try_get("", "id")?;
            let path: String = row.try_get("", "path")?;
            let name = crate::model::library_name::default_name(&path, &used);
            used.insert(name.clone());
            txn.execute(Statement::from_sql_and_values(
                backend,
                "UPDATE library SET name = ? WHERE id = ?",
                [name.into(), id.into()],
            ))
            .await?;
        }
        txn.commit().await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("ALTER TABLE library DROP COLUMN name")
            .await?;
        Ok(())
    }
}
