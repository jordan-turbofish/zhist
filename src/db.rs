use rusqlite::{Connection, Result as SqlResult};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub session: i64,
    pub exit_status: Option<i64>,
    pub start_time: i64,
    pub duration: Option<i64>,
    pub argv: String,
    pub host: String,
    pub dir: String,
}

impl HistoryEntry {
    fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Self> {
        Ok(HistoryEntry {
            id: row.get(0)?,
            session: row.get(1)?,
            exit_status: row.get(2)?,
            start_time: row.get(3)?,
            duration: row.get(4)?,
            argv: row.get(5)?,
            host: row.get(6)?,
            dir: row.get(7)?,
        })
    }

    pub fn query_all(conn: &Connection) -> SqlResult<Vec<Self>> {
        let mut stmt = conn.prepare(
            "SELECT h.id, h.session, h.exit_status, h.start_time, h.duration,
                    c.argv, p.host, p.dir
             FROM history h
             JOIN commands c ON h.command_id = c.id
             JOIN places p ON h.place_id = p.id
             ORDER BY h.start_time DESC",
        )?;
        let rows = stmt.query_map([], Self::from_row)?;
        rows.collect()
    }

    pub fn stream_all(
        conn: &Connection,
        batch_size: usize,
        mut f: impl FnMut(Vec<Self>),
    ) -> SqlResult<()> {
        let mut stmt = conn.prepare(
            "SELECT h.id, h.session, h.exit_status, h.start_time, h.duration,
                    c.argv, p.host, p.dir
             FROM history h
             JOIN commands c ON h.command_id = c.id
             JOIN places p ON h.place_id = p.id
             ORDER BY h.start_time DESC",
        )?;
        let rows = stmt.query_map([], Self::from_row)?;
        let mut chunk = Vec::with_capacity(batch_size);
        for row in rows {
            chunk.push(row?);
            if chunk.len() >= batch_size {
                f(std::mem::take(&mut chunk));
            }
        }
        if !chunk.is_empty() {
            f(chunk);
        }
        Ok(())
    }
}

pub struct HistdbInfo {
    pub host: Option<String>,
    pub session: Option<i64>,
    pub file: Option<String>,
    pub widget: bool,
}

fn strip_quotes(s: String) -> String {
    if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
}

impl HistdbInfo {
    pub fn from_env() -> Self {
        let host = std::env::var("HISTDB_HOST").ok().map(strip_quotes);
        let session = std::env::var("HISTDB_SESSION")
            .ok()
            .and_then(|s| s.parse().ok());
        let file = std::env::var("HISTDB_FILE").ok();
        let widget = std::env::var("ZHIST_WIDGET").is_ok();
        HistdbInfo {
            host,
            session,
            file,
            widget,
        }
    }
}

pub fn open_db(info: &HistdbInfo) -> SqlResult<Connection> {
    let path = info.file.clone().unwrap_or_else(|| {
        let home = std::env::var("HOME").expect("HOME not set");
        format!("{}/.histdb/zsh-history.db", home)
    });
    Connection::open(&path)
}
