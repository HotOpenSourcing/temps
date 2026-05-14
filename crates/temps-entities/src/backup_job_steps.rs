//! Sea-ORM entity for the `backup_job_steps` table (ADR-014).
//!
//! Append-only audit of every step transition, including resume events. Written
//! inside a transaction by `persist_step_completed` in `temps-backup-core`,
//! with the `claim_token` fencing check on the parent `backup_jobs` row.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use temps_core::DBDateTime;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "backup_job_steps")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// FK to the parent `backup_jobs` row.
    pub job_id: i64,
    /// Which attempt of the parent job this step belongs to, for per-attempt
    /// timeline display in the UI.
    pub attempt: i32,
    /// Step name as returned by `BackupEngine::steps()` (e.g. `"upload"`).
    pub step: String,
    /// Transition state: `started` | `completed` | `failed` | `resumed`.
    pub state: String,
    /// Durable cursor the engine wrote at this step. Passed back as
    /// `StepCursor.durable_state` on the next resume so the engine can
    /// reconstruct its position without re-running prior steps.
    pub durable_state: Json,
    /// Human-readable progress note from the engine, if any.
    pub message: Option<String>,
    /// Wall-clock time this step transition was persisted.
    pub occurred_at: DBDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::backup_jobs::Entity",
        from = "Column::JobId",
        to = "super::backup_jobs::Column::Id"
    )]
    BackupJob,
}

impl Related<super::backup_jobs::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::BackupJob.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
