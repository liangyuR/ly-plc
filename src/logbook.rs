use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};

use crate::event::{EventSink, PlcEvent};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Copy)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Category {
    Connection,
    Value,
    Write,
    Edge,
    Error,
    Config,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Category::Connection => "connection",
            Category::Value => "value",
            Category::Write => "write",
            Category::Edge => "edge",
            Category::Error => "error",
            Category::Config => "config",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: i64,
    pub ts: i64,
    pub level: String,
    pub category: String,
    pub point_id: Option<String>,
    pub point_name: Option<String>,
    pub message: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    /// 关联上下文：设置了日志关联点位（[`PlcEngine::set_log_context`](crate::PlcEngine::set_log_context)）时为
    /// 记录这条日志时该点位的值，形如 `requestSeq=12`。
    pub context: Option<String>,
}

impl LogEntry {
    pub fn new(level: Level, category: Category, message: impl Into<String>) -> Self {
        Self {
            id: 0,
            ts: now_ms(),
            level: level.as_str().into(),
            category: category.as_str().into(),
            point_id: None,
            point_name: None,
            message: message.into(),
            old_value: None,
            new_value: None,
            context: None,
        }
    }

    pub fn point(mut self, id: &str, name: &str) -> Self {
        self.point_id = Some(id.into());
        self.point_name = Some(name.into());
        self
    }

    pub fn values(mut self, old: Option<String>, new: Option<String>) -> Self {
        self.old_value = old;
        self.new_value = new;
        self
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogQuery {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub levels: Vec<String>,
    pub categories: Vec<String>,
    pub point_id: Option<String>,
    /// 按关联上下文精确匹配，如 `requestSeq=12`。
    pub context: Option<String>,
    pub keyword: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Serialize)]
pub struct LogPage {
    pub total: i64,
    pub items: Vec<LogEntry>,
}

#[derive(Serialize)]
pub struct HistorySample {
    pub ts: i64,
    pub value: Option<String>,
}

pub struct Logbook {
    tx: Sender<LogEntry>,
    reader: Mutex<Connection>,
    retention_days: Arc<AtomicU32>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS plc_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    level TEXT NOT NULL,
    category TEXT NOT NULL,
    point_id TEXT,
    point_name TEXT,
    message TEXT NOT NULL,
    old_value TEXT,
    new_value TEXT,
    context TEXT
);
CREATE INDEX IF NOT EXISTS idx_plc_log_ts ON plc_log(ts);
CREATE INDEX IF NOT EXISTS idx_plc_log_point ON plc_log(point_id, ts);
";

/// 0.3 及以前建的库没有 context 列：补上空列，旧记录的上下文为空。
fn add_context_column(conn: &Connection) -> rusqlite::Result<()> {
    let present = conn
        .prepare("SELECT 1 FROM pragma_table_info('plc_log') WHERE name = 'context'")?
        .exists([])?;
    if !present {
        conn.execute_batch("ALTER TABLE plc_log ADD COLUMN context TEXT;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_plc_log_context ON plc_log(context, ts);")
}

const COLUMNS: &str =
    "id, ts, level, category, point_id, point_name, message, old_value, new_value, context";

fn open_conn(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
    conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        id: row.get(0)?,
        ts: row.get(1)?,
        level: row.get(2)?,
        category: row.get(3)?,
        point_id: row.get(4)?,
        point_name: row.get(5)?,
        message: row.get(6)?,
        old_value: row.get(7)?,
        new_value: row.get(8)?,
        context: row.get(9)?,
    })
}

impl Logbook {
    pub fn open(path: &Path, sink: EventSink, retention_days: u32) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("创建日志目录失败: {e}"))?;
        }
        let writer = open_conn(path).map_err(|e| format!("打开日志库失败: {e}"))?;
        writer
            .execute_batch(SCHEMA)
            .and_then(|_| add_context_column(&writer))
            .map_err(|e| format!("初始化日志库失败: {e}"))?;
        let reader = open_conn(path).map_err(|e| format!("打开日志库失败: {e}"))?;
        let retention = Arc::new(AtomicU32::new(retention_days));
        let (tx, rx) = mpsc::channel();
        let r = retention.clone();
        std::thread::Builder::new()
            .name("plc-log".into())
            .spawn(move || writer_loop(writer, rx, sink, r))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tx,
            reader: Mutex::new(reader),
            retention_days: retention,
        })
    }

    pub fn log(&self, entry: LogEntry) {
        let _ = self.tx.send(entry);
    }

    pub fn set_retention(&self, days: u32) {
        self.retention_days.store(days, Ordering::Relaxed);
    }

    pub fn query(&self, q: &LogQuery) -> Result<LogPage, String> {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<SqlValue> = Vec::new();
        if let Some(s) = q.start {
            clauses.push("ts >= ?".into());
            args.push(SqlValue::Integer(s));
        }
        if let Some(e) = q.end {
            clauses.push("ts <= ?".into());
            args.push(SqlValue::Integer(e));
        }
        for (column, list) in [("level", &q.levels), ("category", &q.categories)] {
            if !list.is_empty() {
                clauses.push(format!("{column} IN ({})", vec!["?"; list.len()].join(",")));
                args.extend(list.iter().map(|v| SqlValue::Text(v.clone())));
            }
        }
        if let Some(p) = q.point_id.as_ref().filter(|s| !s.is_empty()) {
            clauses.push("point_id = ?".into());
            args.push(SqlValue::Text(p.clone()));
        }
        if let Some(c) = q.context.as_ref().filter(|s| !s.is_empty()) {
            clauses.push("context = ?".into());
            args.push(SqlValue::Text(c.clone()));
        }
        if let Some(k) = q
            .keyword
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            clauses.push("(message LIKE ? OR point_name LIKE ? OR context LIKE ?)".into());
            let pattern = format!("%{k}%");
            args.push(SqlValue::Text(pattern.clone()));
            args.push(SqlValue::Text(pattern.clone()));
            args.push(SqlValue::Text(pattern));
        }
        let filter = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let limit = q.limit.unwrap_or(200).clamp(1, 1000) as i64;
        let offset = q.offset.unwrap_or(0) as i64;

        let conn = self.reader.lock().unwrap();
        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM plc_log {filter}"),
                params_from_iter(args.iter()),
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        args.push(SqlValue::Integer(limit));
        args.push(SqlValue::Integer(offset));
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM plc_log {filter} ORDER BY ts DESC, id DESC LIMIT ? OFFSET ?"
            ))
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params_from_iter(args.iter()), row_to_entry)
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        Ok(LogPage { total, items })
    }

    pub fn point_history(
        &self,
        point_id: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<HistorySample>, String> {
        let conn = self.reader.lock().unwrap();
        let mut samples = Vec::new();
        let baseline = conn.query_row(
            "SELECT new_value FROM plc_log WHERE point_id = ?1 AND category = 'value' AND ts < ?2 ORDER BY ts DESC, id DESC LIMIT 1",
            params![point_id, start],
            |r| r.get::<_, Option<String>>(0),
        );
        if let Ok(value) = baseline {
            samples.push(HistorySample { ts: start, value });
        }
        let mut stmt = conn
            .prepare(
                "SELECT ts, new_value FROM plc_log WHERE point_id = ?1 AND category = 'value' AND ts BETWEEN ?2 AND ?3 ORDER BY ts ASC, id ASC LIMIT 20000",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![point_id, start, end], |r| {
                Ok(HistorySample {
                    ts: r.get(0)?,
                    value: r.get(1)?,
                })
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            samples.push(row.map_err(|e| e.to_string())?);
        }
        Ok(samples)
    }
}

fn purge(conn: &Connection, days: u32) {
    if days == 0 {
        return;
    }
    let cutoff = now_ms() - days as i64 * 86_400_000;
    let _ = conn.execute("DELETE FROM plc_log WHERE ts < ?1", params![cutoff]);
}

fn insert_batch(conn: &mut Connection, batch: &mut [LogEntry]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO plc_log (ts, level, category, point_id, point_name, message, old_value, new_value, context) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for e in batch.iter_mut() {
            stmt.execute(params![
                e.ts,
                e.level,
                e.category,
                e.point_id,
                e.point_name,
                e.message,
                e.old_value,
                e.new_value,
                e.context
            ])?;
            e.id = tx.last_insert_rowid();
        }
    }
    tx.commit()
}

fn writer_loop(
    mut conn: Connection,
    rx: Receiver<LogEntry>,
    sink: EventSink,
    retention: Arc<AtomicU32>,
) {
    let mut last_purge: Option<Instant> = None;
    loop {
        if last_purge.is_none_or(|t| t.elapsed() >= Duration::from_secs(3600)) {
            purge(&conn, retention.load(Ordering::Relaxed));
            last_purge = Some(Instant::now());
        }
        let first = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut batch = vec![first];
        while batch.len() < 500 {
            match rx.try_recv() {
                Ok(e) => batch.push(e),
                Err(_) => break,
            }
        }
        match insert_batch(&mut conn, &mut batch) {
            Ok(()) => sink(PlcEvent::Logs(batch)),
            Err(e) => eprintln!("PLC 日志写入失败: {e}"),
        }
    }
}
