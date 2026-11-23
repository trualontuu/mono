//! Локальная база: история, закладки, пароли, настройки.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

pub struct Store {
    db: Connection,
}

#[derive(Serialize, Clone)]
pub struct Entry {
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub folder: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "is_zero")]
    pub id: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub time: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Serialize, Clone)]
pub struct Login {
    pub id: i64,
    pub host: String,
    pub username: String,
    #[serde(skip)]
    pub blob: Vec<u8>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS history(url TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '', visits INTEGER NOT NULL DEFAULT 1, last INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS history_last ON history(last);
             CREATE INDEX IF NOT EXISTS history_visits ON history(visits);
             CREATE TABLE IF NOT EXISTS bookmarks(id INTEGER PRIMARY KEY, url TEXT NOT NULL, title TEXT NOT NULL, folder TEXT NOT NULL DEFAULT '', pos INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS bookmarks_url ON bookmarks(url);
             CREATE TABLE IF NOT EXISTS logins(id INTEGER PRIMARY KEY, host TEXT NOT NULL, username TEXT NOT NULL, secret BLOB NOT NULL, created INTEGER NOT NULL, UNIQUE(host, username));
             CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        )?;
        Ok(Store { db })
    }

    // ---------- настройки ----------
    pub fn get(&self, k: &str) -> Option<String> {
        self.db
            .query_row("SELECT v FROM kv WHERE k=?1", [k], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }
    pub fn set(&self, k: &str, v: &str) {
        let _ = self
            .db
            .execute("INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=?2", [k, v]);
    }

    // ---------- история ----------
    pub fn visit(&self, url: &str, title: &str) {
        let _ = self.db.execute(
            "INSERT INTO history(url,title,visits,last) VALUES(?1,?2,1,?3)
             ON CONFLICT(url) DO UPDATE SET visits=visits+1, last=?3, title=CASE WHEN ?2<>'' THEN ?2 ELSE title END",
            params![url, title, now()],
        );
    }
    pub fn set_title(&self, url: &str, title: &str) {
        if title.is_empty() {
            return;
        }
        let _ = self
            .db
            .execute("UPDATE history SET title=?2 WHERE url=?1", params![url, title]);
    }
    pub fn import_history(&self, rows: &[(String, String, i64, i64)]) -> usize {
        let tx = match self.db.unchecked_transaction() {
            Ok(t) => t,
            Err(_) => return 0,
        };
        let mut n = 0;
        {
            let mut st = match tx.prepare(
                "INSERT INTO history(url,title,visits,last) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(url) DO UPDATE SET visits=max(visits,?3), last=max(last,?4),
                 title=CASE WHEN title='' THEN ?2 ELSE title END",
            ) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            for (u, t, v, l) in rows {
                if st.execute(params![u, t, v, l]).is_ok() {
                    n += 1;
                }
            }
        }
        let _ = tx.commit();
        n
    }
    pub fn history(&self, q: &str, limit: i64, offset: i64) -> Vec<Entry> {
        let like = format!("%{}%", q.replace('%', "").replace('_', ""));
        let mut st = match self.db.prepare(
            "SELECT url,title,last FROM history WHERE (?1='%%' OR url LIKE ?1 OR title LIKE ?1)
             ORDER BY last DESC LIMIT ?2 OFFSET ?3",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map(params![like, limit, offset], |r| {
            Ok(Entry { url: r.get(0)?, title: r.get(1)?, folder: String::new(), kind: "history", id: 0, time: r.get(2)? })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }
    pub fn delete_history(&self, url: &str) {
        let _ = self.db.execute("DELETE FROM history WHERE url=?1", [url]);
    }
    pub fn clear_history(&self) {
        let _ = self.db.execute("DELETE FROM history", []);
    }
    pub fn top_sites(&self, n: i64) -> Vec<Entry> {
        let mut st = match self.db.prepare(
            "SELECT url,title FROM history WHERE last > ?1 ORDER BY visits DESC LIMIT 200",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows: Vec<(String, String)> = st
            .query_map([now() - 60 * 86400], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|it| it.flatten().collect())
            .unwrap_or_default();
        // по одному сайту на хост
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        for (u, t) in rows {
            let host = host_of(&u);
            if host.is_empty() || !seen.insert(host.clone()) {
                continue;
            }
            let root = format!("{}://{}/", u.split("://").next().unwrap_or("https"), host);
            out.push(Entry { url: root, title: if t.is_empty() { host } else { t }, folder: String::new(), kind: "top", id: 0, time: 0 });
            if out.len() as i64 >= n {
                break;
            }
        }
        out
    }

    /// Подсказки адресной строки: закладки, затем история по частоте.
    pub fn suggest(&self, q: &str, n: usize) -> Vec<Entry> {
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let like = format!("%{}%", q.replace('%', "").replace('_', ""));
        let prefix = |u: &str| {
            let s = u.split("://").nth(1).unwrap_or(u);
            let s = s.strip_prefix("www.").unwrap_or(s);
            s.to_lowercase().starts_with(&q)
        };
        let mut out: Vec<Entry> = vec![];
        if let Ok(mut st) = self.db.prepare(
            "SELECT url,title FROM bookmarks WHERE url LIKE ?1 OR title LIKE ?1 LIMIT 20",
        ) {
            let rows = st
                .query_map([&like], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            for (u, t) in rows {
                out.push(Entry { url: u, title: t, folder: String::new(), kind: "bookmark", id: 0, time: 0 });
            }
        }
        if let Ok(mut st) = self.db.prepare(
            "SELECT url,title FROM history WHERE url LIKE ?1 OR title LIKE ?1 ORDER BY visits DESC, last DESC LIMIT 40",
        ) {
            let rows = st
                .query_map([&like], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.flatten().collect::<Vec<_>>())
                .unwrap_or_default();
            for (u, t) in rows {
                if out.iter().any(|e| e.url == u) {
                    continue;
                }
                out.push(Entry { url: u, title: t, folder: String::new(), kind: "history", id: 0, time: 0 });
            }
        }
        // адреса, начинающиеся с введённого текста — наверх (для автодополнения)
        out.sort_by_key(|e| !prefix(&e.url));
        out.truncate(n);
        out
    }

    // ---------- закладки ----------
    pub fn bookmarks(&self) -> Vec<Entry> {
        let mut st = match self
            .db
            .prepare("SELECT id,url,title,folder FROM bookmarks ORDER BY folder<>'', folder, pos, id")
        {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([], |r| {
            Ok(Entry { id: r.get(0)?, url: r.get(1)?, title: r.get(2)?, folder: r.get(3)?, kind: "bookmark", time: 0 })
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }
    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.db
            .query_row("SELECT 1 FROM bookmarks WHERE url=?1 LIMIT 1", [url], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }
    pub fn add_bookmark(&self, url: &str, title: &str, folder: &str) {
        let pos: i64 = self
            .db
            .query_row("SELECT coalesce(max(pos),0)+1 FROM bookmarks WHERE folder=?1", [folder], |r| r.get(0))
            .unwrap_or(0);
        let _ = self.db.execute(
            "INSERT INTO bookmarks(url,title,folder,pos) VALUES(?1,?2,?3,?4)",
            params![url, title, folder, pos],
        );
    }
    pub fn remove_bookmark_url(&self, url: &str) {
        let _ = self.db.execute("DELETE FROM bookmarks WHERE url=?1", [url]);
    }
    pub fn remove_bookmark(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM bookmarks WHERE id=?1", [id]);
    }
    pub fn rename_bookmark(&self, id: i64, title: &str) {
        let _ = self.db.execute("UPDATE bookmarks SET title=?2 WHERE id=?1", params![id, title]);
    }
    pub fn bookmark_count(&self) -> i64 {
        self.db.query_row("SELECT count(*) FROM bookmarks", [], |r| r.get(0)).unwrap_or(0)
    }

    // ---------- пароли (секрет зашифрован DPAPI) ----------
    pub fn logins_for(&self, host: &str) -> Vec<Login> {
        let mut st = match self
            .db
            .prepare("SELECT id,host,username,secret FROM logins WHERE host=?1 ORDER BY created DESC")
        {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([host], |r| Ok(Login { id: r.get(0)?, host: r.get(1)?, username: r.get(2)?, blob: r.get(3)? }))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }
    pub fn all_logins(&self) -> Vec<Login> {
        let mut st = match self.db.prepare("SELECT id,host,username,secret FROM logins ORDER BY host, username") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        st.query_map([], |r| Ok(Login { id: r.get(0)?, host: r.get(1)?, username: r.get(2)?, blob: r.get(3)? }))
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
    }
    pub fn login_blob(&self, id: i64) -> Option<Vec<u8>> {
        self.db
            .query_row("SELECT secret FROM logins WHERE id=?1", [id], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }
    pub fn save_login(&self, host: &str, user: &str, blob: &[u8]) -> bool {
        self.db
            .execute(
                "INSERT INTO logins(host,username,secret,created) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(host,username) DO UPDATE SET secret=?3, created=?4",
                params![host, user, blob, now()],
            )
            .is_ok()
    }
    pub fn delete_login(&self, id: i64) {
        let _ = self.db.execute("DELETE FROM logins WHERE id=?1", [id]);
    }
}

/// Хост без `www.` — ключ для паролей и плиток.
pub fn host_of(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, r)) => r,
        None => return String::new(),
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map(|(_, h)| h).unwrap_or(host);
    let host = if host.starts_with('[') { host } else { host.split(':').next().unwrap_or(host) };
    host.strip_prefix("www.").unwrap_or(host).to_lowercase()
}
