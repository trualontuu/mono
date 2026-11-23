//! Блокировщик рекламы на движке Brave (adblock-rust).
//! Списки качаются в фоне, движок кэшируется в бинарном виде — старт за миллисекунды.

use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use std::path::{Path, PathBuf};

pub const LISTS: &[(&str, &str)] = &[
    ("easylist", "https://easylist.to/easylist/easylist.txt"),
    ("easyprivacy", "https://easylist.to/easylist/easyprivacy.txt"),
    ("ubo-filters", "https://ublockorigin.github.io/uAssets/filters/filters.txt"),
    ("ubo-privacy", "https://ublockorigin.github.io/uAssets/filters/privacy.txt"),
    ("ubo-badware", "https://ublockorigin.github.io/uAssets/filters/badware.txt"),
    ("ubo-unbreak", "https://ublockorigin.github.io/uAssets/filters/unbreak.txt"),
    ("ubo-quick", "https://ublockorigin.github.io/uAssets/filters/quick-fixes.txt"),
    ("ruadlist", "https://easylist-downloads.adblockplus.org/advblock+cssfixes.txt"),
];
const RESOURCES: &str = "https://raw.githubusercontent.com/brave/adblock-resources/master/dist/resources.json";
const MAX_AGE: u64 = 4 * 86400;
const CACHE_VER: &str = "1";

pub struct Loaded {
    pub engine: Engine,
    pub rules: usize,
}

fn dir(base: &Path) -> PathBuf {
    let d = base.join("adblock");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn age(p: &Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(std::time::SystemTime::now().duration_since(m).ok()?.as_secs())
}

fn fetch(url: &str) -> Option<String> {
    let mut r = ureq::get(url).header("User-Agent", "Mono/0.1").call().ok()?;
    r.body_mut().with_config().limit(32 * 1024 * 1024).read_to_string().ok()
}

/// Загрузка (из кэша или сети). Вызывается в фоновом потоке.
pub fn load(base: &Path, force: bool) -> Option<Loaded> {
    let d = dir(base);
    let cache = d.join("engine.dat");
    let meta = d.join("engine.meta");
    let fresh = age(&cache).map(|a| a < MAX_AGE).unwrap_or(false);
    let meta_ok = std::fs::read_to_string(&meta).ok();
    if !force && fresh {
        if let (Ok(bytes), Some(m)) = (std::fs::read(&cache), meta_ok.as_ref()) {
            let mut parts = m.split(':');
            if parts.next() == Some(CACHE_VER) {
                let rules = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let mut engine = Engine::default();
                if engine.deserialize(&bytes).is_ok() {
                    add_resources(&mut engine, &d, false);
                    return Some(Loaded { engine, rules });
                }
            }
        }
    }
    // Скачиваем свежие списки параллельно; при сбое берём прошлую копию с диска
    let handles: Vec<_> = LISTS
        .iter()
        .map(|(name, url)| {
            let path = d.join(format!("{name}.txt"));
            let url = url.to_string();
            std::thread::spawn(move || {
                if let Some(t) = fetch(&url).filter(|t| t.len() > 1000) {
                    let _ = std::fs::write(&path, &t);
                    Some(t)
                } else {
                    std::fs::read_to_string(&path).ok()
                }
            })
        })
        .collect();
    let mut set = FilterSet::new(false);
    let mut rules = 0;
    for h in handles {
        if let Ok(Some(text)) = h.join() {
            let count = text.lines().filter(|l| !l.is_empty() && !l.starts_with('!') && !l.starts_with('[')).count();
            set.add_filter_list(text, ParseOptions::default());
            rules += count;
        }
    }
    if rules == 0 {
        return None;
    }
    let mut engine = Engine::new_with_filter_set(set);
    let _ = std::fs::write(&cache, engine.serialize());
    let _ = std::fs::write(&meta, format!("{CACHE_VER}:{rules}"));
    add_resources(&mut engine, &d, true);
    Some(Loaded { engine, rules })
}

fn add_resources(engine: &mut Engine, d: &Path, refresh: bool) {
    let p = d.join("resources.json");
    if refresh || !p.exists() {
        if let Some(t) = fetch(RESOURCES) {
            let _ = std::fs::write(&p, t);
        }
    }
    if let Ok(t) = std::fs::read_to_string(&p) {
        if let Ok(res) = serde_json::from_str::<Vec<adblock::resources::Resource>>(&t) {
            engine.use_resources(res);
        }
    }
}

pub enum Verdict {
    Allow,
    Block,
    Redirect(String),
}

pub fn check(engine: &Engine, url: &str, source: &str, kind: &str) -> Verdict {
    let Ok(req) = Request::new(url, source, kind, "GET") else { return Verdict::Allow };
    let r = engine.check_network_request(&req);
    if r.important || (r.filter.is_some() && r.exception.is_none()) {
        match r.redirect {
            Some(data) => Verdict::Redirect(data),
            None => Verdict::Block,
        }
    } else {
        Verdict::Allow
    }
}

/// CSS + скриптлеты для конкретной страницы (вставляются при создании документа).
pub fn cosmetic(engine: &Engine, url: &str) -> (String, String, bool, Vec<String>) {
    let r = engine.url_cosmetic_resources(url);
    let mut css = String::new();
    let sel: Vec<&String> = r.hide_selectors.iter().collect();
    for chunk in sel.chunks(200) {
        css.push_str(&chunk.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","));
        css.push_str("{display:none!important}\n");
    }
    (css, r.injected_script, r.generichide, r.exceptions.into_iter().collect())
}

pub fn generic(engine: &Engine, classes: &[String], ids: &[String], exceptions: &[String]) -> String {
    let ex: std::collections::HashSet<String> = exceptions.iter().cloned().collect();
    let sel = engine.hidden_class_id_selectors(classes, ids, &ex);
    if sel.is_empty() {
        return String::new();
    }
    format!("{}{{display:none!important}}", sel.join(","))
}
