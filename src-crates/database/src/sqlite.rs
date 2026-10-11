use crate::utils::{RowsExt, format_bytes};
use crate::{ChunkInsert, ConnectionInfo, Database, Result, SqliteConfig};
use query::{Query, QueryColumn, Value};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct SqliteConnection {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteConnection {
    pub(crate) async fn test(config: SqliteConfig) -> Result<Option<String>> {
        let conn = Self::make_connection(config.path, config.readonly, config.initial)?;
        // Ensure it's a valid database
        conn.select("SELECT * FROM sqlite_master LIMIT 0;".into())
            .await?;
        conn.select("SELECT concat('SQLite version: ', sqlite_version());".into())
            .await?
            .first_cell_string()
            .map(Some)
    }

    pub(crate) async fn connect(config: SqliteConfig) -> Result<Database> {
        let conn = Self::make_connection(config.path, config.readonly, config.initial)?;
        Ok(Database::Sqlite(conn))
    }

    fn make_connection(path: String, readonly: bool, initial: Option<String>) -> Result<Self> {
        let mut flags = if readonly {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        };
        flags.set(OpenFlags::SQLITE_OPEN_NO_MUTEX, true);
        flags.set(OpenFlags::SQLITE_OPEN_URI, true);
        let conn = Connection::open_with_flags(&path, flags)?;
        if let Some(sql) = initial {
            conn.execute_batch(&sql)?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub(crate) async fn info(&self) -> Result<ConnectionInfo> {
        let [path, version, database_size] = self
            .select(
                "SELECT file, sqlite_version(), CAST(page_count * page_size AS TEXT) FROM pragma_database_list, pragma_page_count('main'), pragma_page_size('main') WHERE name = 'main';".into(),
            )
            .await?
            .first_row_strings::<3>()?;
        let mut info = ConnectionInfo::new("SQLite");
        info.push_db_path(path);
        info.push_text("Size", format_bytes(database_size)?);
        info.push_text("Version", version);
        Ok(info)
    }

    pub(crate) async fn select(&self, sql: String) -> Result<Vec<Vec<Value>>> {
        self.query(sql).await.map(|q| q.rows)
    }

    pub(crate) async fn execute(&self, sql: String) -> Result<()> {
        self.conn.lock().await.execute(&sql, ())?;
        Ok(())
    }

    pub(crate) async fn transaction(&self, sqls: Vec<String>) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        for sql in &sqls {
            tx.execute(sql, [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) async fn query(&self, sql: String) -> Result<Query> {
        let conn = self.conn.lock().await;
        let now = Instant::now();

        let mut stmt = conn.prepare(&sql)?;

        let columns = stmt
            .columns()
            .into_iter()
            .map(|col| QueryColumn {
                name: col.name().into(),
                datatype: col.decl_type().unwrap_or_default().into(),
            })
            .collect::<Vec<_>>();

        let mut values = Vec::new();
        let mut rows = stmt.query(())?;
        while let Some(r) = rows.next()? {
            let mut row = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                let v = decode(r.get_ref(i)?)?;
                row.push(v);
            }
            values.push(row);
        }

        Ok(Query {
            columns,
            rows: values,
            rows_affected: Some(conn.changes()),
            duration: now.elapsed().as_millis() as u32,
        })
    }

    // Builds one UNION ALL query over every table in every attached database that has a
    // `win_rate` column, selecting `win_rate` plus the first 9 other columns of each table
    #[allow(dead_code)]
    pub(crate) async fn high_win_rate_sql(&self, threshold: f64) -> Result<Option<String>> {
        const COLUMNS: usize = 10;
        let mut selects = Vec::new();
        let schemas = self
            .select("SELECT name FROM pragma_database_list ORDER BY seq;".into())
            .await?;
        for [schema] in schemas.into_iter().filter_map(|r| <[Value; 1]>::try_from(r).ok()) {
            let Value::String(schema) = schema else {
                continue;
            };
            let tables = self
                .select(format!(
                    "SELECT name FROM {}.sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%';",
                    quote_id(&schema)
                ))
                .await?;
            for table in tables {
                let Some(Value::String(table)) = table.into_iter().next() else {
                    continue;
                };
                let info = self
                    .select(format!(
                        "SELECT name FROM pragma_table_info({}, {});",
                        quote_str(&table),
                        quote_str(&schema)
                    ))
                    .await?;
                let names = info
                    .into_iter()
                    .filter_map(|r| match r.into_iter().next() {
                        Some(Value::String(n)) => Some(n),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !names.iter().any(|n| n == "win_rate") {
                    continue;
                }
                let picked = std::iter::once("win_rate")
                    .chain(names.iter().map(String::as_str).filter(|n| *n != "win_rate"))
                    .take(COLUMNS)
                    .collect::<Vec<_>>();
                if picked.len() < COLUMNS {
                    continue;
                }
                let cols = picked
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{} AS c{}", quote_id(c), i + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                selects.push(format!(
                    "SELECT {} AS source_db, {} AS source_table, {cols} FROM {}.{} WHERE win_rate > {threshold}",
                    quote_str(&schema),
                    quote_str(&table),
                    quote_id(&schema),
                    quote_id(&table)
                ));
            }
        }
        if selects.is_empty() {
            return Ok(None);
        }
        Ok(Some(selects.join("\nUNION ALL\n") + ";"))
    }

    pub(crate) async fn batch_insert(&self, insert: ChunkInsert) -> Result<()> {
        for sql in insert {
            self.execute(sql).await?;
        }
        Ok(())
    }
}

#[allow(dead_code)]
fn quote_id(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[allow(dead_code)]
fn quote_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn decode(v: ValueRef) -> Result<Value, rusqlite::Error> {
    let v = match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => Value::I64(n),
        ValueRef::Real(f) => Value::F64(f),
        ValueRef::Blob(b) => Value::from_bytes(b.into()),
        ValueRef::Text(t) => {
            // "Only UTF-8 'TEXT' is supported"
            match std::str::from_utf8(t) {
                Ok(s) => Value::String(s.into()),
                Err(err) => return Err(rusqlite::Error::from(err)),
            }
        }
    };
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PLAYER_COLUMNS: &str = "id INTEGER, name TEXT, team TEXT, wins INTEGER, losses INTEGER, \
        games INTEGER, rating REAL, region TEXT, season INTEGER, win_rate REAL";

    fn temp_db(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("sqliteflare_{}_{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn seed(path: &PathBuf, table: &str, rows: &[(i64, &str, f64)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(&format!("CREATE TABLE {table} ({PLAYER_COLUMNS});"))
            .unwrap();
        for (id, name, win_rate) in rows {
            conn.execute(
                &format!("INSERT INTO {table} VALUES (?1, ?2, 't', 9, 1, 10, 1500.0, 'r', 2026, ?3)"),
                rusqlite::params![id, name, win_rate],
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn win_rate_above_90_across_attached_databases() {
        let main_path = temp_db("main");
        let other_path = temp_db("other");
        seed(
            &main_path,
            "players",
            &[(1, "ace", 0.95), (2, "mid", 0.5), (3, "edge", 0.9)],
        );
        seed(&other_path, "champions", &[(4, "star", 0.99), (5, "rookie", 0.1)]);

        let conn = SqliteConnection::make_connection(
            main_path.to_string_lossy().into(),
            false,
            Some(format!(
                "ATTACH DATABASE {} AS other;",
                quote_str(&other_path.to_string_lossy())
            )),
        )
        .unwrap();

        let sql = conn.high_win_rate_sql(0.9).await.unwrap().unwrap();
        let result = conn.query(sql).await.unwrap();

        // source_db + source_table + 10 selected columns
        assert_eq!(result.columns.len(), 12);
        let mut found = result
            .rows
            .iter()
            .map(|r| match (&r[0], &r[1]) {
                (Value::String(db), Value::String(t)) => format!("{db}.{t}"),
                other => panic!("unexpected source columns: {other:?}"),
            })
            .collect::<Vec<_>>();
        found.sort();
        assert_eq!(found, vec!["main.players", "other.champions"]);
        for row in &result.rows {
            match row[2] {
                Value::F64(rate) => assert!(rate > 0.9, "win_rate {rate} not > 0.9"),
                ref other => panic!("win_rate not a float: {other:?}"),
            }
        }

        let _ = std::fs::remove_file(main_path);
        let _ = std::fs::remove_file(other_path);
    }
}
