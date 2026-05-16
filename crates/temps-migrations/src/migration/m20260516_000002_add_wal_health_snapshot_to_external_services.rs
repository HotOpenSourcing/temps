//! Add `wal_health_snapshot` JSONB column to `external_services`.
//!
//! Stores the latest output of the Postgres WAL health probe so the UI can
//! render warnings (stale slots, archive misconfiguration, pg_wal bloat)
//! without re-querying the running database on every page load.
//!
//! Current-state only — history of WAL probes is not retained. The schema
//! lives in `temps-providers::externalsvc::postgres_wal_health` and is
//! serialized via serde_json.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ExternalServices::Table)
                    .add_column(
                        ColumnDef::new(ExternalServices::WalHealthSnapshot)
                            .json_binary()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(ExternalServices::Table)
                    .drop_column(ExternalServices::WalHealthSnapshot)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum ExternalServices {
    Table,
    WalHealthSnapshot,
}
