//! Перенос данных из Google Chrome.

use crate::store::{host_of, Store};
use crate::vault;
use serde_json::Value;
use std::path::PathBuf;

pub fn chrome_profile() -> Option<PathBuf> {
    let base = PathBuf::from(std::env::var("LOCALAPPDATA").ok()?).join("Google\\Chrome\\User Data");
    // Профиль, которым пользовались последним
    let last = std::fs::read_to_string(base.join("Local State"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["profile"]["last_used"].as_str().map(String::from))
        .unwrap_or_else(|| "Default".into());
    let p = base.join(&last);
    if p.is_dir() { Some(p) } else { Some(base.join("Default")).filter(|p| p.is_dir()) }
}

pub fn bookmarks(store: &Store) -> Result<usize, String> {
    let prof = chrome_profile().ok_or("Chrome не найден")?;
    let text = std::fs::read_to_string(prof.join("Bookmarks")).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let existing: std::collections::HashSet<String> = store.bookmarks().into_iter().map(|b| b.url).collect();
    let mut n = 0;
    fn walk(node: &Value, folder: &str, store: &Store, seen: &std::collections::HashSet<String>, n: &mut usize) {
        match node["type"].as_str() {
            Some("url") => {
                let url = node["url"].as_str().unwrap_or("");
                if url.is_empty() || seen.contains(url) || url.starts_with("chrome://") || url.starts_with("javascript:") {
                    return;
                }
                store.add_bookmark(url, node["name"].as_str().unwrap_or(url), folder);
                *n += 1;
            }
            Some("folder") => {
                if let Some(ch) = node["children"].as_array() {
                    for c in ch {
                        let sub = if c["type"] == "folder" {
                            let name = c["name"].as_str().unwrap_or("");
                            if folder.is_empty() { name.to_string() } else { format!("{folder} / {name}") }
                        } else {
                            folder.to_string()
                        };
                        if c["type"] == "folder" {
                            walk(c, &sub, store, seen, n);
                        } else {
                            walk(c, folder, store, seen, n);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let roots = &v["roots"];
    walk(&roots["bookmark_bar"], "", store, &existing, &mut n);
    walk(&roots["other"], "Другие", store, &existing, &mut n);
    walk(&roots["synced"], "Мобильные", store, &existing, &mut n);
    Ok(n)
}

pub fn history(store: &Store) -> Result<usize, String> {
    let prof = chrome_profile().ok_or("Chrome не найден")?;
    // Chrome держит файл открытым — работаем с копией
    let tmp = std::env::temp_dir().join(format!("mono-chrome-history-{}.db", std::process::id()));
    std::fs::copy(prof.join("History"), &tmp).map_err(|e| e.to_string())?;
    let rows = (|| -> rusqlite::Result<Vec<(String, String, i64, i64)>> {
        let db = rusqlite::Connection::open_with_flags(&tmp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut st = db.prepare(
            "SELECT url, title, visit_count, last_visit_time FROM urls
             WHERE hidden=0 AND url LIKE 'http%' ORDER BY last_visit_time DESC LIMIT 60000",
        )?;
        let it = st.query_map([], |r| {
            let t: i64 = r.get(3)?;
            // время Chrome: микросекунды с 1601-01-01
            Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)?.max(1), t / 1_000_000 - 11_644_473_600))
        })?;
        Ok(it.flatten().collect())
    })();
    let _ = std::fs::remove_file(&tmp);
    let rows = rows.map_err(|e| e.to_string())?;
    Ok(store.import_history(&rows))
}

/// CSV, экспортированный из chrome://password-manager/settings
/// (колонки name,url,username,password,note).
pub fn passwords_csv(store: &Store, csv: &str) -> Result<usize, String> {
    let rows = parse_csv(csv);
    let header = rows.first().ok_or("Пустой файл")?;
    let col = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let (cu, cn, cp) = match (col("url"), col("username"), col("password")) {
        (Some(u), Some(n), Some(p)) => (u, n, p),
        _ => return Err("Это не файл паролей Chrome (нет колонок url/username/password)".into()),
    };
    let mut n = 0;
    for r in rows.iter().skip(1) {
        let (Some(url), Some(user), Some(pass)) = (r.get(cu), r.get(cn), r.get(cp)) else { continue };
        let host = host_of(url);
        if host.is_empty() || pass.is_empty() {
            continue;
        }
        if let Some(b) = vault::protect(pass) {
            if store.save_login(&host, user, &b) {
                n += 1;
            }
        }
    }
    Ok(n)
}

fn parse_csv(s: &str) -> Vec<Vec<String>> {
    let mut rows = vec![];
    let mut row = vec![];
    let mut cell = String::new();
    let mut q = false;
    let mut it = s.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = it.next() {
        if q {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    cell.push('"');
                    it.next();
                } else {
                    q = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' => q = true,
            ',' => row.push(std::mem::take(&mut cell)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            _ => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}
