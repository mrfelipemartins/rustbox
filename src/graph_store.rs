use crate::{
    db::Db,
    error::{Error, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{MySql, Row, Transaction};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphPhase {
    Preparing,
    Ready,
    Submitting,
    Accepted,
    Unknown,
    Abandoned,
}

impl GraphPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Ready => "ready",
            Self::Submitting => "submitting",
            Self::Accepted => "accepted",
            Self::Unknown => "unknown",
            Self::Abandoned => "abandoned",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "ready" => Ok(Self::Ready),
            "submitting" => Ok(Self::Submitting),
            "accepted" => Ok(Self::Accepted),
            "unknown" => Ok(Self::Unknown),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(Error::Internal("invalid Graph send phase".into())),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    RetryPreparation,
    UnknownSubmission,
    Terminal,
}

pub fn recovery_action(phase: Option<GraphPhase>) -> RecoveryAction {
    match phase {
        Some(GraphPhase::Preparing | GraphPhase::Ready) => RecoveryAction::RetryPreparation,
        None | Some(GraphPhase::Submitting | GraphPhase::Unknown) => {
            RecoveryAction::UnknownSubmission
        }
        Some(GraphPhase::Accepted | GraphPhase::Abandoned) => RecoveryAction::Terminal,
    }
}

#[derive(Clone)]
pub struct GraphState {
    pub revision: u64,
    pub phase: GraphPhase,
    pub draft_id: Option<String>,
    pub progress: Value,
}

pub async fn load(db: &Db, send_id: &str) -> Result<Option<GraphState>> {
    let Some(row) = sqlx::query(
        "SELECT revision,phase,draft_id,progress FROM graph_send_state WHERE send_id=?",
    )
    .bind(send_id)
    .fetch_optional(&db.pool)
    .await?
    else {
        return Ok(None);
    };
    let bytes: Vec<u8> = row.try_get("progress")?;
    let progress = serde_json::from_slice(&db.crypto.decrypt(&bytes)?)
        .map_err(|_| Error::Internal("invalid Graph upload progress".into()))?;
    Ok(Some(GraphState {
        revision: row.try_get("revision")?,
        phase: GraphPhase::parse(&row.try_get::<String, _>("phase")?)?,
        draft_id: row.try_get_unchecked("draft_id")?,
        progress,
    }))
}

pub async fn create(db: &Db, send_id: &str) -> Result<GraphState> {
    let encrypted = db.crypto.encrypt(b"{}")?;
    sqlx::query("INSERT IGNORE INTO graph_send_state(send_id,progress) VALUES(?,?)")
        .bind(send_id)
        .bind(encrypted)
        .execute(&db.pool)
        .await?;
    load(db, send_id)
        .await?
        .ok_or_else(|| Error::Internal("Graph send state was not created".into()))
}

pub async fn create_in(db: &Db, tx: &mut Transaction<'_, MySql>, send_id: &str) -> Result<()> {
    let encrypted = db.crypto.encrypt(b"{}")?;
    sqlx::query("INSERT INTO graph_send_state(send_id,progress) VALUES(?,?)")
        .bind(send_id)
        .bind(encrypted)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn save(db: &Db, send_id: &str, state: &mut GraphState) -> Result<()> {
    let next_revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::Internal("Graph send revision exhausted".into()))?;
    let bytes = serde_json::to_vec(&state.progress)
        .map_err(|_| Error::Internal("Graph upload progress serialization failed".into()))?;
    let encrypted = db.crypto.encrypt(&bytes)?;
    let mut tx = db.pool.begin().await?;
    let row = sqlx::query("SELECT revision,phase FROM graph_send_state WHERE send_id=? FOR UPDATE")
        .bind(send_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
    let current_revision: u64 = row.try_get("revision")?;
    let current_phase = GraphPhase::parse(&row.try_get::<String, _>("phase")?)?;
    if current_revision != state.revision {
        return Err(Error::Conflict("Graph send progress changed".into()));
    }
    if matches!(
        current_phase,
        GraphPhase::Accepted | GraphPhase::Unknown | GraphPhase::Abandoned
    ) && current_phase != state.phase
    {
        return Err(Error::Conflict(
            "Graph send outcome cannot be retried".into(),
        ));
    }
    sqlx::query("UPDATE graph_send_state SET phase=?,draft_id=?,progress=?,revision=?,updated_at=UTC_TIMESTAMP(6) WHERE send_id=? AND revision=?")
        .bind(state.phase.as_str())
        .bind(&state.draft_id)
        .bind(encrypted)
        .bind(next_revision)
        .bind(send_id)
        .bind(state.revision)
        .execute(&mut *tx)
        .await?;
    if let Some(draft_id) = &state.draft_id {
        sqlx::query("UPDATE sends SET provider_id=? WHERE id=?")
            .bind(draft_id)
            .bind(send_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(message_id) = state
        .progress
        .get("internet_message_id")
        .and_then(Value::as_str)
    {
        sqlx::query("UPDATE sends SET evidence=JSON_SET(COALESCE(evidence,JSON_OBJECT()),'$.provider_message_id',?) WHERE id=?").bind(message_id).bind(send_id).execute(&mut *tx).await?;
    }
    if state.phase == GraphPhase::Submitting && current_phase != GraphPhase::Submitting {
        sqlx::query("UPDATE sends SET submitted_at=UTC_TIMESTAMP(6) WHERE id=?")
            .bind(send_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    state.revision = next_revision;
    Ok(())
}

pub async fn phase(db: &Db, send_id: &str) -> Result<Option<GraphPhase>> {
    let phase: Option<String> =
        sqlx::query_scalar("SELECT phase FROM graph_send_state WHERE send_id=?")
            .bind(send_id)
            .fetch_optional(&db.pool)
            .await?;
    phase.as_deref().map(GraphPhase::parse).transpose()
}

pub async fn clear_upload_secrets(db: &Db, send_id: &str) -> Result<()> {
    let Some(mut state) = load(db, send_id).await? else {
        return Ok(());
    };
    state.progress = json!({});
    save(db, send_id, &mut state).await
}

pub async fn purge_expired_progress(db: &Db) -> Result<()> {
    let empty = db.crypto.encrypt(b"{}")?;
    sqlx::query("UPDATE graph_send_state g JOIN sends s ON s.id=g.send_id SET g.progress=?,g.revision=g.revision+1 WHERE s.expires_at<=UTC_TIMESTAMP(6) AND s.status<>'sending' AND OCTET_LENGTH(g.progress)>?")
        .bind(&empty).bind(empty.len() as u64).execute(&db.pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_distinguishes_uploads_from_final_submission() {
        for phase in [GraphPhase::Preparing, GraphPhase::Ready] {
            assert_eq!(
                recovery_action(Some(phase)),
                RecoveryAction::RetryPreparation
            );
        }
        for phase in [GraphPhase::Submitting, GraphPhase::Unknown] {
            assert_eq!(
                recovery_action(Some(phase)),
                RecoveryAction::UnknownSubmission
            );
        }
        assert_eq!(recovery_action(None), RecoveryAction::UnknownSubmission);
        for phase in [GraphPhase::Accepted, GraphPhase::Abandoned] {
            assert_eq!(recovery_action(Some(phase)), RecoveryAction::Terminal);
        }
    }
}

#[cfg(test)]
mod database_tests {
    use super::*;
    use crate::crypto::Crypto;
    use uuid::Uuid;

    async fn insert_send(tx: &mut Transaction<'_, MySql>, account: &str, id: &str) -> Result<()> {
        sqlx::query("INSERT INTO sends(id,account_id,app_id,idempotency_key,request_hash,status,next_attempt_at,created_at,updated_at,expires_at,message_id,append_status) VALUES(?,?,'graph-store-test',?,?,'queued',UTC_TIMESTAMP(6),UTC_TIMESTAMP(6),UTC_TIMESTAMP(6),UTC_TIMESTAMP(6)+INTERVAL 24 HOUR,'<graph-store@test>','disabled')")
            .bind(id).bind(account).bind(id).bind(vec![0u8;32]).execute(&mut **tx).await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a dedicated MySQL database in TEST_DATABASE_URL"]
    async fn progress_is_atomic_encrypted_revision_guarded_and_submission_safe()
    -> anyhow::Result<()> {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(5)
            .connect(&std::env::var("TEST_DATABASE_URL")?)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        let db = Db::new(pool, Crypto::new(&[42; 32])?);
        let input = serde_json::from_value(
            json!({"name":"Graph progress store","imap":{"host":"mail.example.com","port":993,"username":"test","password":"test"},"smtp":{"host":"mail.example.com","port":465,"username":"test","password":"test"}}),
        )?;
        let account = db.create_account(input).await?;
        let id = Uuid::new_v4().to_string();
        let mut tx = db.pool.begin().await?;
        insert_send(&mut tx, &account.id, &id).await?;
        create_in(&db, &mut tx, &id).await?;
        tx.rollback().await?;
        assert!(load(&db, &id).await?.is_none());

        let mut tx = db.pool.begin().await?;
        insert_send(&mut tx, &account.id, &id).await?;
        create_in(&db, &mut tx, &id).await?;
        tx.commit().await?;
        let mut state = load(&db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("progress missing"))?;
        assert_eq!(state.revision, 1);
        assert_eq!(state.phase, GraphPhase::Preparing);
        let mut stale = state.clone();
        state.draft_id = Some("Draft-CaseSensitive-AbC".into());
        state.progress = json!({"session":{"url":"https://outlook.office.com/upload?authtoken=private-upload-secret","offset":3276800}});
        state.phase = GraphPhase::Ready;
        save(&db, &id, &mut state).await?;
        assert_eq!(state.revision, 2);
        stale.progress = json!({"session":"stale"});
        assert!(matches!(
            save(&db, &id, &mut stale).await,
            Err(Error::Conflict(_))
        ));
        let loaded = load(&db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("progress missing"))?;
        assert_eq!(loaded.draft_id.as_deref(), Some("Draft-CaseSensitive-AbC"));
        assert_eq!(loaded.progress["session"]["offset"], 3276800);
        let encrypted: Vec<u8> =
            sqlx::query_scalar("SELECT progress FROM graph_send_state WHERE send_id=?")
                .bind(&id)
                .fetch_one(&db.pool)
                .await?;
        assert!(!String::from_utf8_lossy(&encrypted).contains("private-upload-secret"));
        let provider_id: Option<String> =
            sqlx::query_scalar("SELECT provider_id FROM sends WHERE id=?")
                .bind(&id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(provider_id.as_deref(), Some("Draft-CaseSensitive-AbC"));
        assert_eq!(
            recovery_action(phase(&db, &id).await?),
            RecoveryAction::RetryPreparation
        );

        let not_submitted: Option<chrono::NaiveDateTime> =
            sqlx::query_scalar("SELECT submitted_at FROM sends WHERE id=?")
                .bind(&id)
                .fetch_one(&db.pool)
                .await?;
        assert!(not_submitted.is_none());
        state.phase = GraphPhase::Submitting;
        save(&db, &id, &mut state).await?;
        let submitted: Option<chrono::NaiveDateTime> =
            sqlx::query_scalar("SELECT submitted_at FROM sends WHERE id=?")
                .bind(&id)
                .fetch_one(&db.pool)
                .await?;
        assert!(submitted.is_some());
        assert_eq!(
            recovery_action(phase(&db, &id).await?),
            RecoveryAction::UnknownSubmission
        );
        state.phase = GraphPhase::Unknown;
        save(&db, &id, &mut state).await?;
        state.phase = GraphPhase::Preparing;
        assert!(matches!(
            save(&db, &id, &mut state).await,
            Err(Error::Conflict(_))
        ));
        clear_upload_secrets(&db, &id).await?;
        let loaded = load(&db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("progress missing"))?;
        assert_eq!(loaded.phase, GraphPhase::Unknown);
        assert_eq!(loaded.progress, json!({}));
        assert_eq!(loaded.draft_id.as_deref(), Some("Draft-CaseSensitive-AbC"));

        sqlx::query("UPDATE sends SET status='unknown',expires_at=UTC_TIMESTAMP(6)-INTERVAL 1 SECOND WHERE id=?").bind(&id).execute(&db.pool).await?;
        let mut expiry_state = load(&db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Graph state missing"))?;
        expiry_state.progress =
            json!({"session":{"url":"https://outlook.office.com/upload?authtoken=expired-secret"}});
        save(&db, &id, &mut expiry_state).await?;
        purge_expired_progress(&db).await?;
        let retained = load(&db, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Graph metadata missing"))?;
        assert_eq!(retained.progress, json!({}));
        assert_eq!(retained.phase, GraphPhase::Unknown);
        purge_expired_progress(&db).await?;
        assert_eq!(
            load(&db, &id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Graph metadata missing"))?
                .revision,
            retained.revision
        );
        db.delete_account(&account.id).await?;
        Ok(())
    }
}
