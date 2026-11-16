//! SQLite persistence for parse jobs, MinerU key stats and structured logs.
//!
//! Kept in an independent `parse.db` inside the parse server data directory
//! (not the SAG index database): the parse server must run without the
//! desktop indexer, and its schema must evolve without index migrations.
use super::types::{now_unix, AttemptRecord, JobStatus, ParseJobRecord};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyStat {
    pub provider: String,
    pub slot: i64,
    pub success: i64,
    pub failure: i64,
    pub pages: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooled_until: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: i64,
    pub ts: i64,
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

pub struct ParseStore {
    db: Mutex<Connection>,
    #[allow(dead_code)]
    path: PathBuf,
}

fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<ParseJobRecord> {
    let status: String = row.get("status")?;
    let attempts_json: String = row.get("attempts")?;
    Ok(ParseJobRecord {
        task_id: row.get("task_id")?,
        input_md5: row.get("input_md5")?,
        input_file: row.get("input_file")?,
        output_file: row.get("output_file")?,
        client_id: row.get("client_id")?,
        engine: row.get("engine")?,
        status: JobStatus::from_str(&status).unwrap_or(JobStatus::Pending),
        prompt: row.get("prompt")?,
        attempts: serde_json::from_str(&attempts_json).unwrap_or_default(),
        error: row.get("error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        pages: row.get("pages")?,
        progress: row.get("progress")?,
    })
}

impl ParseStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path).context("无法打开解析任务库")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS parse_jobs (
                task_id TEXT PRIMARY KEY,
                input_md5 TEXT NOT NULL,
                input_file TEXT NOT NULL,
                output_file TEXT NOT NULL,
                client_id TEXT,
                engine TEXT NOT NULL,
                status TEXT NOT NULL,
                prompt TEXT,
                attempts TEXT NOT NULL DEFAULT '[]',
                error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                started_at INTEGER,
                finished_at INTEGER,
                pages INTEGER,
                progress TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_parse_jobs_status ON parse_jobs(status);
            CREATE INDEX IF NOT EXISTS idx_parse_jobs_md5 ON parse_jobs(input_md5);
            CREATE TABLE IF NOT EXISTS mineru_key_stats (
                provider TEXT NOT NULL,
                slot INTEGER NOT NULL,
                success INTEGER NOT NULL DEFAULT 0,
                failure INTEGER NOT NULL DEFAULT 0,
                pages INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                cooled_until INTEGER,
                PRIMARY KEY (provider, slot)
            );
            CREATE TABLE IF NOT EXISTS parse_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                level TEXT NOT NULL,
                task_id TEXT,
                client_id TEXT,
                message TEXT NOT NULL,
                engine TEXT,
                elapsed_ms INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_parse_logs_task ON parse_logs(task_id);
            CREATE INDEX IF NOT EXISTS idx_parse_logs_ts ON parse_logs(ts);",
        )?;
        Ok(Self {
            db: Mutex::new(db),
            path: path.to_path_buf(),
        })
    }

    pub fn insert(&self, record: &ParseJobRecord) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO parse_jobs (task_id,input_md5,input_file,output_file,client_id,engine,status,prompt,attempts,error,created_at,updated_at,started_at,finished_at,pages,progress)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                record.task_id,
                record.input_md5,
                record.input_file,
                record.output_file,
                record.client_id,
                record.engine,
                record.status.as_str(),
                record.prompt,
                serde_json::to_string(&record.attempts)?,
                record.error,
                record.created_at,
                record.updated_at,
                record.started_at,
                record.finished_at,
                record.pages,
                record.progress,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, task_id: &str) -> Result<Option<ParseJobRecord>> {
        let db = self.db.lock().unwrap();
        Ok(db
            .query_row(
                "SELECT task_id,input_md5,input_file,output_file,client_id,engine,status,prompt,attempts,error,created_at,updated_at,started_at,finished_at,pages,progress FROM parse_jobs WHERE task_id=?1",
                params![task_id],
                row_to_record,
            )
            .optional()?)
    }

    pub fn get_by_md5(&self, md5: &str) -> Result<Option<ParseJobRecord>> {
        let db = self.db.lock().unwrap();
        Ok(db
            .query_row(
                "SELECT task_id,input_md5,input_file,output_file,client_id,engine,status,prompt,attempts,error,created_at,updated_at,started_at,finished_at,pages,progress FROM parse_jobs WHERE input_md5=?1 ORDER BY created_at DESC LIMIT 1",
                params![md5],
                row_to_record,
            )
            .optional()?)
    }

    /// Paginated task list for `GET /api/tasks`, newest first.
    /// `status` must already be validated by the caller (one of
    /// pending/processing/completed/failed); `search` matches
    /// `input_file` or `task_id` (LIKE wildcards in the term are escaped).
    /// Returns `(page_rows, total_matching_rows)`.
    pub fn list(
        &self,
        status: Option<&str>,
        client_id: Option<&str>,
        search: Option<&str>,
        page: usize,
        page_size: usize,
    ) -> Result<(Vec<ParseJobRecord>, usize)> {
        let db = self.db.lock().unwrap();
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(status) = status {
            conditions.push("status=?".into());
            params.push(rusqlite::types::Value::Text(status.to_string()));
        }
        if let Some(client_id) = client_id {
            conditions.push("client_id=?".into());
            params.push(rusqlite::types::Value::Text(client_id.to_string()));
        }
        if let Some(search) = search {
            let escaped = search
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            conditions.push("(input_file LIKE ? ESCAPE '\\' OR task_id LIKE ? ESCAPE '\\')".into());
            let pattern = format!("%{escaped}%");
            params.push(rusqlite::types::Value::Text(pattern.clone()));
            params.push(rusqlite::types::Value::Text(pattern));
        }
        let where_sql = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let total: usize = db.query_row(
            &format!("SELECT COUNT(*) FROM parse_jobs{where_sql}"),
            rusqlite::params_from_iter(params.iter().map(|v| v as &dyn rusqlite::types::ToSql)),
            |row| row.get(0),
        )?;
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let offset = (page - 1) * page_size;
        let sql = format!(
            "SELECT task_id,input_md5,input_file,output_file,client_id,engine,status,prompt,attempts,error,created_at,updated_at,started_at,finished_at,pages,progress FROM parse_jobs{where_sql} ORDER BY created_at DESC, task_id DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(page_size as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params.iter().map(|v| v as &dyn rusqlite::types::ToSql)),
                row_to_record,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((rows, total))
    }

    pub fn delete(&self, task_id: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM parse_jobs WHERE task_id=?1", params![task_id])?;
        Ok(())
    }

    pub fn update_status(
        &self,
        task_id: &str,
        status: JobStatus,
        error: Option<&str>,
    ) -> Result<()> {
        let now = now_unix();
        let db = self.db.lock().unwrap();
        let finished: Option<i64> = match status {
            JobStatus::Completed | JobStatus::Failed => Some(now),
            _ => None,
        };
        let started: Option<i64> = match status {
            JobStatus::Processing => Some(now),
            _ => None,
        };
        if started.is_some() {
            db.execute(
                "UPDATE parse_jobs SET status=?1, error=?2, updated_at=?3, started_at=COALESCE(started_at,?4), finished_at=NULL WHERE task_id=?5",
                params![status.as_str(), error, now, started, task_id],
            )?;
        } else if finished.is_some() {
            db.execute(
                "UPDATE parse_jobs SET status=?1, error=?2, updated_at=?3, finished_at=?4 WHERE task_id=?5",
                params![status.as_str(), error, now, finished, task_id],
            )?;
        } else {
            db.execute(
                "UPDATE parse_jobs SET status=?1, error=?2, updated_at=?3 WHERE task_id=?4",
                params![status.as_str(), error, now, task_id],
            )?;
        }
        Ok(())
    }

    /// Persist one `Attempt` callback: append the attempt, refresh the live
    /// progress snapshot and the page count when a segment reports it.
    pub fn record_attempt(&self, task_id: &str, attempt: &AttemptRecord) -> Result<()> {
        let mut record = self.get(task_id)?.context("任务不存在")?;
        record.attempts.push(attempt.clone());
        if let Some(total) = attempt.segment_total {
            record.pages = Some(total);
        }
        let progress = serde_json::json!({
            "engine": attempt.engine,
            "phase": attempt.phase,
            "segment_current": attempt.segment_current,
            "segment_total": attempt.segment_total,
            "elapsed_ms": attempt.elapsed_ms,
        })
        .to_string();
        record.progress = Some(progress);
        record.updated_at = now_unix();
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE parse_jobs SET attempts=?1, pages=?2, progress=?3, updated_at=?4 WHERE task_id=?5",
            params![
                serde_json::to_string(&record.attempts)?,
                record.pages,
                record.progress,
                record.updated_at,
                task_id,
            ],
        )?;
        Ok(())
    }

    pub fn update_engine(&self, task_id: &str, engine: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE parse_jobs SET engine=?1, updated_at=?2 WHERE task_id=?3",
            params![engine, now_unix(), task_id],
        )?;
        Ok(())
    }

    pub fn reset_for_restart(&self, task_id: &str, engine: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE parse_jobs SET status='pending', error=NULL, engine=?1, started_at=NULL, finished_at=NULL, progress=NULL, updated_at=?2 WHERE task_id=?3",
            params![engine, now_unix(), task_id],
        )?;
        Ok(())
    }

    pub fn unfinished(&self) -> Result<Vec<ParseJobRecord>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT task_id,input_md5,input_file,output_file,client_id,engine,status,prompt,attempts,error,created_at,updated_at,started_at,finished_at,pages,progress FROM parse_jobs WHERE status IN ('pending','processing') ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn counts(&self) -> Result<serde_json::Value> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT status, COUNT(*) FROM parse_jobs GROUP BY status")?;
        let mut counts = serde_json::json!({"pending":0,"processing":0,"completed":0,"failed":0});
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
            counts[status] = serde_json::json!(count);
        }
        Ok(counts)
    }

    // -- MinerU key stats -------------------------------------------------

    pub fn key_stat(&self, provider: &str, slot: i64) -> Result<KeyStat> {
        let db = self.db.lock().unwrap();
        Ok(db
            .query_row(
                "SELECT provider,slot,success,failure,pages,last_error,cooled_until FROM mineru_key_stats WHERE provider=?1 AND slot=?2",
                params![provider, slot],
                |row| {
                    Ok(KeyStat {
                        provider: row.get(0)?,
                        slot: row.get(1)?,
                        success: row.get(2)?,
                        failure: row.get(3)?,
                        pages: row.get(4)?,
                        last_error: row.get(5)?,
                        cooled_until: row.get(6)?,
                    })
                },
            )
            .optional()?
            .unwrap_or(KeyStat {
                provider: provider.into(),
                slot,
                success: 0,
                failure: 0,
                pages: 0,
                last_error: None,
                cooled_until: None,
            }))
    }

    pub fn record_key_outcome(
        &self,
        provider: &str,
        slot: i64,
        success: bool,
        pages: u32,
        error: Option<&str>,
        cooled_until: Option<i64>,
    ) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO mineru_key_stats (provider,slot,success,failure,pages,last_error,cooled_until) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(provider,slot) DO UPDATE SET
               success=success+excluded.success, failure=failure+excluded.failure,
               pages=pages+excluded.pages, last_error=excluded.last_error,
               cooled_until=excluded.cooled_until",
            params![
                provider,
                slot,
                if success { 1 } else { 0 },
                if success { 0 } else { 1 },
                if success { pages as i64 } else { 0 },
                error,
                cooled_until,
            ],
        )?;
        Ok(())
    }

    // -- Structured logs ---------------------------------------------------

    pub fn log(
        &self,
        level: &str,
        task_id: Option<&str>,
        client_id: Option<&str>,
        message: &str,
        engine: Option<&str>,
        elapsed_ms: Option<u64>,
    ) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO parse_logs (ts,level,task_id,client_id,message,engine,elapsed_ms) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                now_unix(),
                level,
                task_id,
                client_id,
                message,
                engine,
                elapsed_ms.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn logs(
        &self,
        level: Option<&str>,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LogEntry>> {
        let db = self.db.lock().unwrap();
        let mut sql = String::from(
            "SELECT id,ts,level,task_id,client_id,message,engine,elapsed_ms FROM parse_logs WHERE 1=1",
        );
        if level.is_some() {
            sql.push_str(" AND level=?");
        }
        if task_id.is_some() {
            sql.push_str(" AND task_id=?");
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        let mut stmt = db.prepare(&sql)?;
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(level) = level {
            params.push(rusqlite::types::Value::Text(level.to_string()));
        }
        if let Some(task_id) = task_id {
            params.push(rusqlite::types::Value::Text(task_id.to_string()));
        }
        params.push((limit.min(1000) as i64).into());
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params.iter().map(|v| v as &dyn rusqlite::types::ToSql)),
                |row| {
                    Ok(LogEntry {
                        id: row.get(0)?,
                        ts: row.get(1)?,
                        level: row.get(2)?,
                        task_id: row.get(3)?,
                        client_id: row.get(4)?,
                        message: row.get(5)?,
                        engine: row.get(6)?,
                        elapsed_ms: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn prune_logs(&self, retention_days: u32) -> Result<usize> {
        let cutoff = now_unix() - retention_days as i64 * 86400;
        let db = self.db.lock().unwrap();
        Ok(db.execute("DELETE FROM parse_logs WHERE ts < ?1", params![cutoff])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_server::types::JobStatus;

    fn record(id: &str) -> ParseJobRecord {
        ParseJobRecord {
            task_id: id.into(),
            input_md5: "abc".into(),
            input_file: "ab/cd/abc.pdf".into(),
            output_file: "temp/ab/cd/abc.md".into(),
            client_id: Some("c1".into()),
            engine: "mineru".into(),
            status: JobStatus::Pending,
            prompt: None,
            attempts: vec![],
            error: None,
            created_at: now_unix(),
            updated_at: now_unix(),
            started_at: None,
            finished_at: None,
            pages: None,
            progress: None,
        }
    }

    #[test]
    fn job_lifecycle_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParseStore::open(&dir.path().join("parse.db")).unwrap();
        store.insert(&record("t1")).unwrap();
        assert_eq!(store.get("t1").unwrap().unwrap().engine, "mineru");
        store
            .update_status("t1", JobStatus::Processing, None)
            .unwrap();
        let row = store.get("t1").unwrap().unwrap();
        assert_eq!(row.status, JobStatus::Processing);
        assert!(row.started_at.is_some());
        store
            .record_attempt(
                "t1",
                &AttemptRecord {
                    engine: "MinerU".into(),
                    phase: None,
                    segment_current: Some(1),
                    segment_total: Some(3),
                    elapsed_ms: 42,
                    error: None,
                },
            )
            .unwrap();
        let row = store.get("t1").unwrap().unwrap();
        assert_eq!(row.pages, Some(3));
        assert_eq!(row.attempts.len(), 1);
        store
            .update_status("t1", JobStatus::Completed, None)
            .unwrap();
        assert!(store.get("t1").unwrap().unwrap().finished_at.is_some());
        assert!(store.unfinished().unwrap().is_empty());
        store
            .log(
                "info",
                Some("t1"),
                Some("c1"),
                "done",
                Some("MinerU"),
                Some(42),
            )
            .unwrap();
        let logs = store.logs(None, Some("t1"), 10).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].message, "done");
    }

    #[test]
    fn list_filters_search_and_paginates() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParseStore::open(&dir.path().join("parse.db")).unwrap();
        // Newest first: created_at 300 > 200 > 100.
        for (id, status, client, input, created) in [
            (
                "t1",
                JobStatus::Completed,
                Some("c1"),
                "aa/bb/report.pdf",
                100,
            ),
            ("t2", JobStatus::Failed, Some("c2"), "cc/dd/notes.docx", 200),
            (
                "t3",
                JobStatus::Completed,
                Some("c1"),
                "ee/ff/report.pdf",
                300,
            ),
            ("t4", JobStatus::Pending, None, "gg/hh/scan.png", 150),
        ] {
            let mut rec = record(id);
            rec.status = status;
            rec.client_id = client.map(str::to_string);
            rec.input_file = input.into();
            rec.created_at = created;
            rec.updated_at = created;
            store.insert(&rec).unwrap();
        }
        let (rows, total) = store.list(None, None, None, 1, 20).unwrap();
        assert_eq!(total, 4);
        assert_eq!(
            rows.iter().map(|r| r.task_id.as_str()).collect::<Vec<_>>(),
            vec!["t3", "t2", "t4", "t1"]
        );
        let (rows, total) = store.list(Some("completed"), None, None, 1, 20).unwrap();
        assert_eq!(total, 2);
        assert_eq!(
            rows.iter().map(|r| r.task_id.as_str()).collect::<Vec<_>>(),
            vec!["t3", "t1"]
        );
        let (rows, total) = store.list(None, Some("c1"), None, 1, 20).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows[0].task_id, "t3");
        let (rows, total) = store.list(None, None, Some("report"), 1, 20).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows[0].task_id, "t3");
        // A literal `%` in the search term must not act as a wildcard.
        let (rows, total) = store.list(None, None, Some("rep%rt"), 1, 20).unwrap();
        assert_eq!((total, rows.len()), (0, 0));
        let (rows, total) = store.list(None, None, None, 2, 3).unwrap();
        assert_eq!(total, 4);
        assert_eq!(
            rows.iter().map(|r| r.task_id.as_str()).collect::<Vec<_>>(),
            vec!["t1"]
        );
        // Unknown status / client simply match nothing.
        let (_, total) = store.list(Some("bogus"), None, None, 1, 20).unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn key_stats_accumulate_and_cool_down() {
        let dir = tempfile::tempdir().unwrap();
        let store = ParseStore::open(&dir.path().join("parse.db")).unwrap();
        store
            .record_key_outcome("official", 0, true, 5, None, None)
            .unwrap();
        store
            .record_key_outcome("official", 0, false, 0, Some("429"), Some(9999999999))
            .unwrap();
        let stat = store.key_stat("official", 0).unwrap();
        assert_eq!((stat.success, stat.failure, stat.pages), (1, 1, 5));
        assert_eq!(stat.cooled_until, Some(9999999999));
    }
}
