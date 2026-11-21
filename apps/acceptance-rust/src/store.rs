use std::{path::PathBuf, time::Duration};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

const APPLICATION_ID: i64 = 0x4349_5459;
const VERSION: i64 = 8;

#[derive(Clone)]
pub(crate) struct Store(PathBuf);

impl Store {
    pub(crate) fn initialize(path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut db = Connection::open(&path)?;
        db.busy_timeout(Duration::from_secs(3))?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let application = pragma(&tx, "application_id")?;
        let version = pragma(&tx, "user_version")?;
        if application == 0 && version == 0 {
            let objects: i64 = tx.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if objects != 0 {
                return Err("请为城线指定独立数据库。".into());
            }
            tx.execute_batch(include_str!("media.sql"))?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        } else if application != APPLICATION_ID || !(1..=VERSION).contains(&version) {
            return Err("数据库版本不适用于城线。".into());
        }
        if version < 2 {
            tx.execute_batch(include_str!("media/tasks.sql"))?;
        }
        if version < 3 {
            tx.execute_batch(include_str!("operations.sql"))?;
        }
        if version < 4 {
            tx.execute_batch(include_str!("media/provenance.sql"))?;
        }
        if version < 5 {
            tx.execute_batch(
                "DROP INDEX media_snapshots_refresh_id;
                 CREATE UNIQUE INDEX media_snapshots_refresh_id ON media_snapshots(refresh_id, starts, ends);",
            )?;
        }
        if version < 6 {
            anonymize_legacy_food_records(&tx)?;
        }
        if version < 7 {
            initialize_evidence_lineage(&tx)?;
        }
        if version < 8 {
            tx.execute_batch(
                "ALTER TABLE media_tasks ADD COLUMN updated_at TEXT;
                 WITH latest AS (
                     SELECT task_id, max(created_at) AS updated_at FROM audit_events
                     WHERE task_id IS NOT NULL AND category IN ('任务', 'Agent', '人工审核')
                     GROUP BY task_id
                 )
                 UPDATE media_tasks SET updated_at=latest.updated_at FROM latest
                 WHERE media_tasks.id=latest.task_id;
                 CREATE TRIGGER media_tasks_touch_after_update
                 AFTER UPDATE OF active, payload, report, review, review_note, deleted ON media_tasks
                 WHEN OLD.active IS NOT NEW.active OR OLD.payload IS NOT NEW.payload
                   OR OLD.report IS NOT NEW.report OR OLD.review IS NOT NEW.review
                   OR OLD.review_note IS NOT NEW.review_note OR OLD.deleted IS NOT NEW.deleted
                 BEGIN
                     UPDATE media_tasks SET updated_at=CURRENT_TIMESTAMP WHERE id=NEW.id;
                 END;",
            )?;
        }
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.execute(
            "INSERT INTO audit_events (category, action, task_id, detail)
             SELECT '任务', '执行中断', id, '服务重启时任务仍在执行。' FROM media_tasks WHERE active=1", [],
        )?;
        tx.execute("UPDATE media_tasks SET active=0 WHERE active=1", [])?;
        tx.execute(
            "INSERT INTO audit_events (category, action, detail) VALUES ('API 服务', '启动', '本地工作台 API 已初始化。')",
            [],
        )?;
        tx.commit()?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        Ok(Self(path))
    }

    pub(crate) fn connect(&self) -> rusqlite::Result<Connection> {
        let db = Connection::open_with_flags(&self.0, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_secs(3))?;
        if pragma(&db, "application_id")? != APPLICATION_ID
            || pragma(&db, "user_version")? != VERSION
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(db)
    }
}

fn initialize_evidence_lineage(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "ALTER TABLE media_snapshots ADD COLUMN evidence_root TEXT;
         CREATE INDEX media_snapshots_evidence_root ON media_snapshots(evidence_root);",
    )?;
    let mut audit = tx.prepare(
        "SELECT detail FROM audit_events WHERE category='平台' AND action='补充线路和触达证据' ORDER BY id",
    )?;
    let records = audit.query_map([], |row| row.get::<_, String>(0))?;
    let mut attach = tx.prepare(
        "UPDATE media_snapshots AS child
         SET evidence_root=coalesce(parent.evidence_root, parent.id)
         FROM media_snapshots AS parent
         WHERE parent.id=?1 AND child.id=?2 AND child.rowid>parent.rowid
           AND child.evidence_root IS NULL AND child.refresh_id IS NULL
           AND child.industry=parent.industry AND child.starts=parent.starts
           AND child.ends=parent.ends AND child.observed_at=parent.observed_at
           AND child.source=parent.source AND child.media_mask=parent.media_mask",
    )?;
    for record in records {
        let record = record?;
        if let Some((old, new)) = record
            .strip_prefix("原快照 ")
            .and_then(|detail| detail.split_once(" · 新快照 "))
            && let (Ok(old), Ok(new)) = (old.parse::<uuid::Uuid>(), new.parse::<uuid::Uuid>())
        {
            attach.execute(params![old.to_string(), new.to_string()])?;
        }
    }
    Ok(())
}

fn anonymize_legacy_food_records(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    for (table, column) in [
        ("media_snapshots", "payload"),
        ("media_tasks", "payload"),
        ("media_tasks", "report"),
        ("media_tasks", "review_note"),
        ("audit_events", "detail"),
    ] {
        let mut query = tx.prepare(&format!(
            "SELECT rowid, {column} FROM {table} WHERE {column} LIKE ?1 OR {column} LIKE ?2 OR {column} LIKE ?3"
        ))?;
        let matched = query
            .query_map(
                [
                    "%\u{661f}\u{5df4}\u{514b}%",
                    "%\u{0073}tarbucks%",
                    "%\u{70d8}\u{7119}\u{5de5}\u{574a}%",
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut update = tx.prepare(&format!("UPDATE {table} SET {column}=?1 WHERE rowid=?2"))?;
        for (id, previous) in matched {
            let clean = crate::media::anonymize_food_case(&previous);
            if clean.as_ref() != previous {
                update.execute(rusqlite::params![clean.as_ref(), id])?;
            }
        }
    }
    Ok(())
}

fn pragma(db: &Connection, name: &str) -> rusqlite::Result<i64> {
    db.pragma_query_value(None, name, |row| row.get(0))
}
