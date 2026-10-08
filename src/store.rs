#![forbid(unsafe_code)]
use crate::{
    config::Paths,
    protocol::{Job, Request},
};
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;

pub struct Store {
    connection: Connection,
}
impl Store {
    pub fn open(paths: &Paths) -> Result<Self> {
        let _ = crate::platform::private_file(&paths.database)?;
        let connection = Connection::open(&paths.database)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > 1 {
            bail!("queue database schema is newer than this executable; use a compatible version");
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; CREATE TABLE IF NOT EXISTS jobs (sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, data TEXT NOT NULL); CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL); PRAGMA user_version=1;")?;
        Ok(Self { connection })
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        let mut statement = self
            .connection
            .prepare("SELECT sequence,data FROM jobs ORDER BY sequence")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut jobs = Vec::new();
        for row in rows {
            let (sequence, json) = row?;
            let mut job: Job = serde_json::from_str(&json)?;
            job.sequence = sequence as u64;
            jobs.push(job);
        }
        Ok(jobs)
    }
    pub fn register(&self, request: Request) -> Result<Job> {
        if let Some(job) = self
            .jobs()?
            .into_iter()
            .find(|job| job.request.id == request.id)
        {
            if job.request.owner != request.owner {
                bail!("request ownership mismatch");
            }
            return Ok(job);
        }
        let job = Job {
            sequence: 0,
            request,
            state: "queued".into(),
            child: None,
            server: None,
            code: None,
            cancel: false,
        };
        self.connection.execute(
            "INSERT INTO jobs(id,data) VALUES(?1,?2)",
            params![job.request.id, serde_json::to_string(&job)?],
        )?;
        Ok(Job {
            sequence: self.connection.last_insert_rowid() as u64,
            ..job
        })
    }
    pub fn save(&self, job: &Job) -> Result<()> {
        self.connection.execute(
            "UPDATE jobs SET data=?1 WHERE sequence=?2",
            params![serde_json::to_string(job)?, job.sequence as i64],
        )?;
        Ok(())
    }
    pub fn drained(&self) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM settings WHERE key='drained'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default()
            == "true")
    }
    pub fn set_drained(&self, value: bool) -> Result<()> {
        self.connection.execute("INSERT INTO settings(key,value) VALUES('drained',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![value.to_string()])?;
        Ok(())
    }
    pub fn trim(&self) -> Result<()> {
        self.connection.execute("DELETE FROM jobs WHERE sequence NOT IN (SELECT sequence FROM jobs ORDER BY sequence DESC LIMIT 500) AND json_extract(data,'$.state') IN ('finished','cancelled')",[])?;
        Ok(())
    }
    pub fn legacy_shims(&self) -> Result<Vec<PathBuf>> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM settings WHERE key='legacy_shims'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(json
            .map(|value| serde_json::from_str(&value))
            .transpose()?
            .unwrap_or_default())
    }
    pub fn track_legacy_shims(&self, paths: Vec<PathBuf>) -> Result<()> {
        if paths.iter().any(|path| !path.is_absolute()) {
            bail!("legacy shim paths must be absolute");
        }
        let mut combined = self.legacy_shims()?;
        combined.extend(paths);
        combined.sort();
        combined.dedup();
        self.connection.execute("INSERT INTO settings(key,value) VALUES('legacy_shims',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![serde_json::to_string(&combined)?])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drain_state_read_errors_cannot_become_permission_to_admit() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: root.path().into(),
            config: root.path().join("config.toml"),
            socket: root.path().join("control.sock"),
            database: root.path().join("queue.sqlite3"),
        };
        let store = Store::open(&paths).unwrap();
        assert!(!store.drained().unwrap());
        store.set_drained(true).unwrap();
        assert!(store.drained().unwrap());
        store
            .connection
            .execute_batch("DROP TABLE settings")
            .unwrap();
        assert!(store.drained().is_err());
    }
}
