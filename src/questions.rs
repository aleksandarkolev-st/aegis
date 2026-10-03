//! Durable user questions, separate from model summaries and archived events.
use anyhow::{Result, bail};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde_json::json;

use crate::storage::{Store, append_event};

#[derive(Debug, Serialize)]
pub struct Question {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct AnsweredQuestion {
    pub question: String,
    pub answer: String,
}

pub(crate) fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS user_questions (
        id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id),
        text TEXT NOT NULL, answer TEXT, created_at INTEGER NOT NULL
    ); CREATE INDEX IF NOT EXISTS pending_user_questions ON user_questions(run_id, created_at);
    CREATE TABLE IF NOT EXISTS user_question_waits (
        run_id TEXT PRIMARY KEY REFERENCES runs(id), waiting INTEGER NOT NULL CHECK(waiting IN (0,1))
    );",
    )?;
    Ok(())
}

pub(crate) fn track_state(
    transaction: &Transaction<'_>,
    run_id: &str,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    if matches!(
        kind,
        "run.created"
            | "run.ready"
            | "run.running"
            | "run.paused"
            | "run.waiting_recovery"
            | "run.failed"
            | "run.cancelled"
            | "run.answered"
            | "run.completed"
    ) {
        let waiting = kind == "run.waiting_recovery" && payload["reason"] == "awaiting user answer";
        transaction.execute("INSERT INTO user_question_waits(run_id,waiting) VALUES (?1,?2) ON CONFLICT(run_id) DO UPDATE SET waiting=excluded.waiting", params![run_id,waiting])?;
    }
    Ok(())
}

pub(crate) fn restore_waits(store: &Store) -> Result<()> {
    let mut query = store.connection.prepare("SELECT id FROM runs WHERE state='waiting_recovery' AND EXISTS(SELECT 1 FROM user_questions WHERE run_id=runs.id AND answer IS NULL) AND NOT EXISTS(SELECT 1 FROM user_question_waits WHERE run_id=runs.id)")?;
    let runs = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for run_id in runs {
        // Migration reads integrity-checked archives as well as hot events.
        // Missing or unrelated reasons never authorize automatic resumption.
        let waiting = store
            .events(&run_id)?
            .into_iter()
            .rev()
            .find(|event| event.kind == "run.waiting_recovery")
            .is_some_and(|event| event.payload["reason"] == "awaiting user answer");
        store.connection.execute(
            "INSERT OR IGNORE INTO user_question_waits(run_id,waiting) VALUES (?1,?2)",
            params![run_id, waiting],
        )?;
    }
    Ok(())
}

pub(crate) fn has_pending(connection: &Connection, run_id: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM user_questions WHERE run_id=?1 AND answer IS NULL)",
        [run_id],
        |row| row.get(0),
    )?)
}

pub(crate) fn waiting_for_answer(connection: &Connection, run_id: &str) -> Result<bool> {
    Ok(has_pending(connection, run_id)? && connection.query_row(
        "SELECT state='waiting_recovery' AND COALESCE((SELECT waiting FROM user_question_waits WHERE run_id=?1),0)=1 FROM runs WHERE id=?1",
        [run_id], |row| row.get::<_, bool>(0))?)
}

pub(crate) fn validate_resolved(connection: &Connection, run_id: &str) -> Result<()> {
    if has_pending(connection, run_id)? {
        bail!("Answer the pending user questions before completing this task");
    }
    Ok(())
}

impl Store {
    pub fn question_answers(&self, run_id: &str) -> Result<Vec<AnsweredQuestion>> {
        let mut query = self.connection.prepare("SELECT text,answer FROM user_questions WHERE run_id=?1 AND answer IS NOT NULL ORDER BY created_at,rowid")?;
        let rows = query.query_map([run_id], |row| {
            Ok(AnsweredQuestion {
                question: row.get(0)?,
                answer: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn pending_questions(&self, run_id: &str) -> Result<Vec<Question>> {
        let mut query = self.connection.prepare("SELECT id,text FROM user_questions WHERE run_id=?1 AND answer IS NULL ORDER BY created_at,rowid LIMIT 3")?;
        let rows = query.query_map([run_id], |row| {
            Ok(Question {
                id: row.get(0)?,
                text: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn waiting_for_answer(&self, run_id: &str) -> Result<bool> {
        waiting_for_answer(&self.connection, run_id)
    }

    pub fn ask_user(&mut self, run_id: &str, question: &str) -> Result<()> {
        let question = crate::text::clean(question).trim().to_owned();
        if question.is_empty() || question.len() > 1600 {
            bail!("A question must contain 1..1600 bytes of text");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state: String =
            transaction.query_row("SELECT state FROM runs WHERE id=?1", [run_id], |row| {
                row.get(0)
            })?;
        if state != "running" {
            bail!("Only a running task can ask a question");
        }
        let pending: usize = transaction.query_row(
            "SELECT COUNT(*) FROM user_questions WHERE run_id=?1 AND answer IS NULL",
            [run_id],
            |row| row.get(0),
        )?;
        if pending >= 3 {
            bail!(
                "Three questions are already awaiting answers; continue independent work or wait for the user"
            );
        }
        let duplicate: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM user_questions WHERE run_id=?1 AND text=?2 AND answer IS NULL)", params![run_id,question], |row| row.get(0))?;
        if duplicate {
            bail!("This question is already awaiting an answer");
        }
        let id = uuid::Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO user_questions(id,run_id,text,created_at) VALUES (?1,?2,?3,?4)",
            params![id, run_id, question, crate::storage::unix_time()],
        )?;
        append_event(
            &transaction,
            run_id,
            "user.question",
            json!({"id":id,"text":question}),
        )?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archiving_wait_events_preserves_question_resume_and_unsafe_wait_boundaries() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Choose",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.ask_user(&run.id, "Which format?")?;
        store.state(
            &run.id,
            "waiting_recovery",
            json!({"reason":"awaiting user answer"}),
        )?;
        for _ in 0..300 {
            store.event(&run.id, "telemetry", json!({}))?;
        }
        store.save_snapshot(&run.id)?;
        assert!(store.archive_history(&run.id)? > 0);
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert!(store.waiting_for_answer(&run.id)?);
        // Simulate a database from the release before the durable wait projection.
        store
            .connection
            .execute("DELETE FROM user_question_waits WHERE run_id=?1", [&run.id])?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert!(store.waiting_for_answer(&run.id)?);
        store.steer(&run.id, "JSON")?;
        assert_eq!(store.run(&run.id)?.state, "ready");
        store.state(&run.id, "running", json!({}))?;
        store.ask_user(&run.id, "Which destination?")?;
        store.state(
            &run.id,
            "waiting_recovery",
            json!({"reason":"unsafe operation needs reconciliation"}),
        )?;
        for _ in 0..300 {
            store.event(&run.id, "telemetry", json!({}))?;
        }
        store.save_snapshot(&run.id)?;
        store.archive_history(&run.id)?;
        store
            .connection
            .execute("DELETE FROM user_question_waits WHERE run_id=?1", [&run.id])?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert!(!store.waiting_for_answer(&run.id)?);
        assert!(store.steer(&run.id, "Continue").is_err());
        assert_eq!(store.pending_questions(&run.id)?.len(), 1);
        Ok(())
    }

    #[test]
    fn questions_survive_restart_and_answers_resume_only_the_question_wait() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let run = store.create_run(
            "Choose a format",
            directory.path(),
            "custom",
            json!([]),
            json!({}),
            "",
        )?;
        store.state(&run.id, "running", json!({}))?;
        store.ask_user(&run.id, "Which output format?")?;
        assert!(store.ask_user(&run.id, "Which output format?").is_err());
        assert!(store.answer_run(&run.id, "Done").is_err());
        store.state(
            &run.id,
            "waiting_recovery",
            json!({"reason":"awaiting user answer"}),
        )?;
        drop(store);
        let mut store = Store::open(directory.path())?;
        assert!(store.waiting_for_answer(&run.id)?);
        assert_eq!(
            store.pending_questions(&run.id)?[0].text,
            "Which output format?"
        );
        store.steer(&run.id, "JSON please")?;
        assert!(store.pending_questions(&run.id)?.is_empty());
        assert_eq!(store.question_answers(&run.id)?[0].answer, "JSON please");
        assert_eq!(store.run(&run.id)?.state, "ready");
        assert_eq!(
            store.pending_steering(&run.id)?[0].payload["text"],
            "JSON please"
        );
        store.state(&run.id, "running", json!({}))?;
        store.ask_user(&run.id, "Which destination?")?;
        store.state(
            &run.id,
            "waiting_recovery",
            json!({"reason":"unsafe operation needs reconciliation"}),
        )?;
        assert!(!store.waiting_for_answer(&run.id)?);
        assert!(store.steer(&run.id, "Continue").is_err());
        assert_eq!(store.pending_questions(&run.id)?.len(), 1);
        Ok(())
    }
}
