use crate::MetaStore;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use scion_core::ObjectId;
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS changesets (
    seq  INTEGER PRIMARY KEY AUTOINCREMENT,
    id   TEXT NOT NULL UNIQUE,
    data BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS trees (
    id   TEXT PRIMARY KEY,
    data BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS refs (
    name    TEXT PRIMARY KEY,
    target  TEXT NOT NULL,
    version INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS derived (
    kind TEXT NOT NULL,
    key  TEXT NOT NULL,
    data BLOB NOT NULL,
    PRIMARY KEY (kind, key)
) WITHOUT ROWID;
";

/// SQLite metadata store. `changesets.seq` makes the table an append-only log.
pub struct SqliteMeta {
    conn: Mutex<Connection>,
}

impl SqliteMeta {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteMeta {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn parse(s: String) -> Result<ObjectId> {
    Ok(ObjectId::from_hex(&s)?)
}

impl MetaStore for SqliteMeta {
    fn append_changeset(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO changesets (id, data) VALUES (?1, ?2)",
            params![id.to_hex(), data],
        )?;
        Ok(())
    }

    fn get_changeset(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT data FROM changesets WHERE id = ?1",
                [id.to_hex()],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn find_changesets_by_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<ObjectId>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id FROM changesets WHERE id LIKE ?1 || '%' LIMIT ?2")?;
        let ids = stmt
            .query_map(params![prefix, limit as i64], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(parse).collect()
    }

    fn put_tree(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO trees (id, data) VALUES (?1, ?2)",
            params![id.to_hex(), data],
        )?;
        Ok(())
    }

    fn get_tree(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row("SELECT data FROM trees WHERE id = ?1", [id.to_hex()], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn get_ref(&self, name: &str) -> Result<Option<ObjectId>> {
        self.conn()
            .query_row("SELECT target FROM refs WHERE name = ?1", [name], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(parse)
            .transpose()
    }

    fn cas_ref(
        &self,
        name: &str,
        expected: Option<ObjectId>,
        new: Option<ObjectId>,
    ) -> Result<bool> {
        let conn = self.conn();
        let changed = match (expected, new) {
            (None, Some(n)) => conn.execute(
                "INSERT OR IGNORE INTO refs (name, target, version) VALUES (?1, ?2, 1)",
                params![name, n.to_hex()],
            )?,
            (Some(e), Some(n)) => conn.execute(
                "UPDATE refs SET target = ?3, version = version + 1 WHERE name = ?1 AND target = ?2",
                params![name, e.to_hex(), n.to_hex()],
            )?,
            (Some(e), None) => conn.execute(
                "DELETE FROM refs WHERE name = ?1 AND target = ?2",
                params![name, e.to_hex()],
            )?,
            (None, None) => {
                let exists: Option<i64> = conn
                    .query_row("SELECT 1 FROM refs WHERE name = ?1", [name], |r| r.get(0))
                    .optional()?;
                return Ok(exists.is_none());
            }
        };
        Ok(changed == 1)
    }

    fn list_refs(&self) -> Result<Vec<(String, ObjectId)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT name, target FROM refs ORDER BY name")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(n, t)| Ok((n, parse(t)?))).collect()
    }

    fn get_derived(&self, kind: &str, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT data FROM derived WHERE kind = ?1 AND key = ?2",
                [kind, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn put_derived(&self, kind: &str, key: &str, data: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO derived (kind, key, data) VALUES (?1, ?2, ?3)",
            params![kind, key, data],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_cas() {
        let m = SqliteMeta::in_memory().unwrap();
        let (a, b) = (ObjectId::hash(b"a"), ObjectId::hash(b"b"));
        assert!(m.cas_ref("main", None, Some(a)).unwrap());
        assert!(
            !m.cas_ref("main", None, Some(b)).unwrap(),
            "create must fail when ref exists"
        );
        assert!(
            !m.cas_ref("main", Some(b), Some(a)).unwrap(),
            "stale expected value"
        );
        assert!(m.cas_ref("main", Some(a), Some(b)).unwrap());
        assert_eq!(m.get_ref("main").unwrap(), Some(b));
        assert_eq!(m.list_refs().unwrap(), vec![("main".to_string(), b)]);
        assert!(m.cas_ref("main", Some(b), None).unwrap());
        assert_eq!(m.get_ref("main").unwrap(), None);
    }

    #[test]
    fn changesets_and_prefix() {
        let m = SqliteMeta::in_memory().unwrap();
        let id = ObjectId::hash(b"cs");
        m.append_changeset(&id, b"{}").unwrap();
        m.append_changeset(&id, b"{}").unwrap();
        assert_eq!(m.get_changeset(&id).unwrap().unwrap(), b"{}");
        assert_eq!(
            m.find_changesets_by_prefix(&id.to_hex()[..8], 2).unwrap(),
            vec![id]
        );
    }
}
