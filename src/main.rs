#![windows_subsystem = "windows"]
//! Mono — минималистичный ч/б браузер на Rust поверх WebView2.

mod blocker;
mod import;
mod store;
mod vault;
mod wv;

use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use store::{host_of, Entry, Store};
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::*;
use windows::core::{w, Interface, BOOL, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::Win32::UI::WindowsAndMessaging::*;
use wv::{get_str, h, View};

const BAR_DIP: f64 = 76.0;
const STRIP_DIP: f64 = 4.0;
const WM_WAKE: u32 = WM_APP + 1;
const WM_DRAG: u32 = WM_APP + 2;
const WM_WINCMD: u32 = WM_APP + 3;
const T_FLUSH: usize = 1;
const T_TICK: usize = 2;
const INTERNAL: &str = "https://mono.local/";
const NEWTAB: &str = "https://mono.local/newtab";

static UI_HTML: &str = include_str!("../ui/ui.html");
static POPUP_HTML: &str = include_str!("../ui/popup.html");
static NEWTAB_HTML: &str = include_str!("../ui/newtab.html");
static PAGE_HTML: &str = include_str!("../ui/page.html");
static CONTENT_JS: &str = include_str!("../ui/content.js");
static YT_JS: &str = include_str!("../ui/youtube.js");

static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

pub enum Msg {
    Engine(Option<blocker::Loaded>),
    Suggest(u64, Vec<String>),
    Note(String),
}

fn wake() {
    let h = MAIN_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as _)), WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

// ---------------------------------------------------------------- состояние

struct Tab {
    id: u64,
    view: Option<View>,
    creating: bool,
    url: String,
    nav_url: String,
    title: String,
    fav: String,
    loading: bool,
    blocked: u32,
    audio: bool,
    muted: bool,
    hidden_at: Option<Instant>,
    suspended: bool,
    cos_script: Option<String>,
    cos_ex: Vec<String>,
    zoom: f64,
}

impl Tab {
    fn new(id: u64, url: &str) -> Tab {
        Tab {
            id,
            view: None,
            creating: false,
            url: url.into(),
            nav_url: url.into(),
            title: String::new(),
            fav: String::new(),
            loading: false,
            blocked: 0,
            audio: false,
            muted: false,
            hidden_at: None,
            suspended: false,
            cos_script: None,
            cos_ex: vec![],
            zoom: 1.0,
        }
    }
}

struct Search {
    key: &'static str,
    name: &'static str,
    url: &'static str,
    sug: &'static str,
}

const SEARCH: &[Search] = &[
    Search { key: "google", name: "Google", url: "https://www.google.com/search?q={q}", sug: "https://suggestqueries.google.com/complete/search?client=firefox&ie=utf-8&oe=utf-8&hl=ru&q={q}" },
    Search { key: "yandex", name: "Яндекс", url: "https://ya.ru/search/?text={q}", sug: "https://suggest.yandex.ru/suggest-ff.cgi?uil=ru&part={q}" },
    Search { key: "duck", name: "DuckDuckGo", url: "https://duckduckgo.com/?q={q}", sug: "https://duckduckgo.com/ac/?type=list&q={q}" },
    Search { key: "bing", name: "Bing", url: "https://www.bing.com/search?q={q}", sug: "https://api.bing.com/osjson.aspx?query={q}" },
];

struct App {
    hwnd: HWND,
    popup_hwnd: HWND,
    env: Option<ICoreWebView2Environment>,
    ui: Option<View>,
    popup: Option<View>,
    popup_on: bool,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    closed: Vec<String>,
    store: Store,
    data: PathBuf,
    engine: Option<adblock::Engine>,
    rules: usize,
    total_blocked: u64,
    adblock_on: bool,
    allow: HashSet<String>,
    search: usize,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    sug_tx: mpsc::Sender<(u64, String)>,
    sug_seq: u64,
    sug_q: String,
    sug_local: Vec<Value>,
    fullscreen: bool,
    saved_style: isize,
    saved_rect: RECT,
    dirty: bool,
    session_dirty: bool,
    bg_click: Option<Instant>,
    pw_pending: Option<(String, String, String)>,
    last_state: String,
    bar_on: bool,
    bar_n: usize,
}

thread_local! {
    static NAV_T0: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static QUEUE: RefCell<Vec<Box<dyn FnOnce(&mut App)>>> = const { RefCell::new(Vec::new()) };
}

/// Выполнить действие над состоянием. Если состояние занято (вложенный вызов) —
/// отложить до освобождения.
fn app(f: impl FnOnce(&mut App) + 'static) {
    let ran = APP.with(|c| match c.try_borrow_mut() {
        Ok(mut g) => {
            if let Some(a) = g.as_mut() {
                f(a);
            }
            true
        }
        Err(_) => {
            QUEUE.with(|q| q.borrow_mut().push(Box::new(f)));
            false
        }
    });
    if ran {
        drain_queue();
    } else {
        wake();
    }
}

fn drain_queue() {
    loop {
        let next = QUEUE.with(|q| {
            let mut q = q.borrow_mut();
            if q.is_empty() { None } else { Some(q.remove(0)) }
        });
        let Some(f) = next else { break };
        let ok = APP.with(|c| match c.try_borrow_mut() {
            Ok(mut g) => {
                if let Some(a) = g.as_mut() {
                    f(a);
                }
                true
            }
            Err(_) => {
                QUEUE.with(|q| q.borrow_mut().insert(0, f));
                false
            }
        });
        if !ok {
            break;
        }
    }
}

/// Синхронный доступ (нужен там, где WebView2 ждёт ответ прямо в обработчике).
fn try_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// ---------------------------------------------------------------- адреса

fn urlenc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn looks_like_host(s: &str) -> bool {
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    if host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    let Some((_, tld)) = host.rsplit_once('.') else { return false };
    tld.chars().count() >= 2
        && tld.chars().all(|c| c.is_alphabetic() || c == '-')
        && !host.starts_with('.')
        && host.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '.')
}

impl App {
    fn search_url(&self, q: &str) -> String {
        SEARCH[self.search].url.replace("{q}", &urlenc(q))
    }

    fn resolve(&self, input: &str) -> String {
        let s = input.trim();
        if s.is_empty() {
            return NEWTAB.into();
        }
        let lower = s.to_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file:") || lower.starts_with("about:") || lower.starts_with("data:") || lower.starts_with("view-source:") {
            return s.into();
        }
        if s.len() > 2 && s.as_bytes()[1] == b':' && (s.as_bytes()[2] == b'\\' || s.as_bytes()[2] == b'/') {
            return format!("file:///{}", s.replace('\\', "/"));
        }
        if !s.contains(' ') && looks_like_host(s) {
            let host = s.split(['/', ':']).next().unwrap_or("");
            let plain = host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok();
            return format!("{}://{}", if plain { "http" } else { "https" }, s);
        }
        self.search_url(s)
    }
}

// ---------------------------------------------------------------- окно и раскладка

fn scale(hwnd: HWND) -> f64 {
    unsafe { GetDpiForWindow(hwnd) as f64 / 96.0 }
}

fn client(hwnd: HWND) -> RECT {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut r);
    }
    r
}

fn zoomed(hwnd: HWND) -> bool {
    unsafe { IsZoomed(hwnd).as_bool() }
}

impl App {
    fn top_strip(&self) -> i32 {
        if self.fullscreen || zoomed(self.hwnd) { 0 } else { (STRIP_DIP * scale(self.hwnd)).round() as i32 }
    }

    fn chrome_dip(&self) -> f64 {
        BAR_DIP + if self.bar_on && self.bar_n > 0 { 32.0 } else { 0.0 }
    }

    fn content_rect(&self) -> RECT {
        let r = client(self.hwnd);
        let top = if self.fullscreen { 0 } else { self.top_strip() + (self.chrome_dip() * scale(self.hwnd)).round() as i32 };
        RECT { left: 0, top, right: r.right, bottom: r.bottom }
    }

    fn layout(&mut self) {
        let r = client(self.hwnd);
        let s = scale(self.hwnd);
        let top = self.top_strip();
        if let Some(ui) = &self.ui {
            if self.fullscreen {
                ui.visible(false);
            } else {
                ui.visible(true);
                ui.bounds(RECT { left: 0, top, right: r.right, bottom: top + (self.chrome_dip() * s).round() as i32 });
            }
        }
        let cr = self.content_rect();
        if let Some(t) = self.tabs.get(self.active) {
            if let Some(v) = &t.view {
                v.bounds(cr);
            }
        }
        self.hide_popup();
        self.dirty();
    }

    fn dirty(&mut self) {
        if !self.dirty {
            self.dirty = true;
            unsafe {
                SetTimer(Some(self.hwnd), T_FLUSH, 30, None);
            }
        }
    }

    fn set_fullscreen(&mut self, on: bool) {
        if on == self.fullscreen {
            return;
        }
        unsafe {
            if on {
                self.saved_style = GetWindowLongPtrW(self.hwnd, GWL_STYLE);
                let _ = GetWindowRect(self.hwnd, &mut self.saved_rect);
                let mon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
                let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                let _ = GetMonitorInfoW(mon, &mut mi);
                self.fullscreen = true;
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                let m = mi.rcMonitor;
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOP), m.left, m.top, m.right - m.left, m.bottom - m.top, SWP_FRAMECHANGED | SWP_NOOWNERZORDER);
            } else {
                self.fullscreen = false;
                SetWindowLongPtrW(self.hwnd, GWL_STYLE, self.saved_style);
                let r = self.saved_rect;
                let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER);
            }
        }
        self.layout();
    }

    // ---------------------------------------------------------------- состояние для панели

    fn flush(&mut self) {
        self.dirty = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_FLUSH);
        }
        let Some(ui) = self.ui.clone() else { return };
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": if t.title.is_empty() { display_title(&t.url) } else { t.title.clone() },
                    "fav": t.fav,
                    "loading": t.loading,
                    "audio": t.audio,
                    "muted": t.muted,
                    "internal": t.url.starts_with(INTERNAL),
                })
            })
            .collect();
        let (url, back, fwd, blocked, id, zoom) = match self.tabs.get(self.active) {
            Some(t) => {
                let (b, f) = t.view.as_ref().map(|v| (v.can_back(), v.can_fwd())).unwrap_or((false, false));
                (t.url.clone(), b, f, t.blocked, t.id, t.zoom)
            }
            None => (String::new(), false, false, 0, 0, 1.0),
        };
        let host = host_of(&url);
        let internal = url.starts_with(INTERNAL);
        let state = json!({
            "t": "state",
            "tabs": tabs,
            "active": id,
            "url": if internal { String::new() } else { url.clone() },
            "internal": internal,
            "back": back,
            "fwd": fwd,
            "loading": self.tabs.get(self.active).map(|t| t.loading).unwrap_or(false),
            "blocked": blocked,
            "shield": self.adblock_on && !self.allow.contains(&host),
            "adblock": self.adblock_on,
            "bm": !internal && !url.is_empty() && self.store.is_bookmarked(&url),
            "max": zoomed(self.hwnd),
            "zoom": zoom,
        })
        .to_string();
        if state != self.last_state {
            ui.post(&state);
            self.last_state = state;
        }
        // заголовок окна — для панели задач и Alt+Tab
        if let Some(t) = self.tabs.get(self.active) {
            let title = if t.title.is_empty() { "Mono".to_string() } else { format!("{} — Mono", t.title) };
            unsafe {
                let _ = SetWindowTextW(self.hwnd, &h(&title));
            }
        }
    }

    // ---------------------------------------------------------------- панель быстрого доступа

    fn send_bar(&mut self) {
        let bms = self.store.bookmarks();
        let mut items: Vec<Value> = vec![];
        let mut folders: Vec<String> = vec![];
        for b in &bms {
            if b.folder.is_empty() {
                items.push(json!({"k": "link", "id": b.id, "url": b.url, "title": b.title}));
            } else {
                let top = b.folder.split(" / ").next().unwrap_or("").to_string();
                if top == "Другие" || top == "Мобильные" || folders.contains(&top) {
                    continue;
                }
                folders.push(top);
            }
        }
        for f in folders {
            items.push(json!({"k": "folder", "name": f}));
        }
        let n = items.len();
        if let Some(ui) = &self.ui {
            ui.post(&json!({"t": "bar", "on": self.bar_on, "items": items}).to_string());
        }
        if n != self.bar_n {
            self.bar_n = n;
            self.layout();
        }
    }

    fn folder_items(&self, name: &str) -> Vec<Value> {
        let sub = format!("{name} / ");
        self.store
            .bookmarks()
            .into_iter()
            .filter(|b| b.folder == name || b.folder.starts_with(&sub))
            .map(|b| json!({"kind": "bookmark", "title": b.title, "url": b.url, "group": b.folder.strip_prefix(&sub).unwrap_or("")}))
            .collect()
    }

    // ---------------------------------------------------------------- вкладки

    fn idx(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn tab(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_view(&self) -> Option<View> {
        self.tabs.get(self.active).and_then(|t| t.view.clone())
    }

    fn new_tab(&mut self, url: &str, at: Option<usize>, activate: bool) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let pos = at.unwrap_or(self.tabs.len()).min(self.tabs.len());
        self.tabs.insert(pos, Tab::new(id, url));
        if pos <= self.active && !self.tabs.is_empty() && self.tabs.len() > 1 {
            self.active += 1;
        }
        if activate {
            self.select(pos);
        } else {
            self.create_view(id, None);
        }
        self.session_dirty = true;
        self.dirty();
        id
    }

    fn select(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let cr = self.content_rect();
        if i != self.active {
            if let Some(old) = self.tabs.get_mut(self.active) {
                if let Some(v) = &old.view {
                    v.visible(false);
                    set_mem(v, false);
                }
                old.hidden_at = Some(Instant::now());
            }
        }
        self.active = i;
        let t = &mut self.tabs[i];
        t.hidden_at = None;
        t.suspended = false;
        let id = t.id;
        let url = t.url.clone();
        match t.view.clone() {
            Some(v) => {
                v.bounds(cr);
                v.visible(true);
                set_mem(&v, true);
                if url.starts_with(NEWTAB) { self.focus_address("") } else { v.focus() }
            }
            None => {
                if !t.creating {
                    self.create_view(id, None);
                }
                if url.starts_with(NEWTAB) {
                    self.focus_address("");
                }
            }
        }
        self.hide_popup();
        self.session_dirty = true;
        self.dirty();
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        let t = self.tabs.remove(i);
        if !t.url.starts_with(NEWTAB) {
            self.closed.push(t.url.clone());
        }
        if let Some(v) = t.view {
            v.close();
        }
        if self.tabs.is_empty() {
            self.save_session();
            unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let was_active = i == self.active;
        if i < self.active || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if was_active {
            // Как в Chrome: справа, если есть, иначе слева
            let next = i.min(self.tabs.len() - 1);
            self.active = usize::MAX;
            self.select(next);
        }
        self.session_dirty = true;
        self.dirty();
    }

    fn navigate_active(&mut self, url: &str) {
        let Some(t) = self.tabs.get_mut(self.active) else { return };
        t.url = url.into();
        t.nav_url = url.into();
        match &t.view {
            Some(v) => {
                v.navigate(url);
                v.focus();
            }
            None => {
                let id = t.id;
                if !t.creating {
                    self.create_view(id, None);
                }
            }
        }
        self.hide_popup();
        self.dirty();
    }

    fn focus_address(&mut self, text: &str) {
        if let Some(ui) = &self.ui {
            ui.focus();
            ui.post(&json!({"t": "focusaddr", "text": text}).to_string());
        }
    }

    // ---------------------------------------------------------------- создание WebView

    fn create_view(&mut self, id: u64, on_ready: Option<Box<dyn FnOnce(&View)>>) {
        let Some(env) = self.env.clone() else { return };
        if let Some(t) = self.tab(id) {
            t.creating = true;
        }
        let hwnd = self.hwnd;
        let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |res, ctl| {
            res?;
            let Some(ctl) = ctl else { return Ok(()) };
            let wv = unsafe { ctl.CoreWebView2()? };
            let view = View { ctl, wv };
            setup_content(&view, id);
            let pending_open = on_ready.is_some();
            if let Some(f) = on_ready {
                f(&view);
            }
            app(move |a| a.view_ready(id, view, pending_open));
            Ok(())
        }));
        unsafe {
            let _ = env.CreateCoreWebView2Controller(hwnd, &handler);
        }
    }

    fn view_ready(&mut self, id: u64, view: View, opened_by_page: bool) {
        let cr = self.content_rect();
        let active_id = self.tabs.get(self.active).map(|t| t.id);
        let Some(t) = self.tab(id) else {
            view.close();
            return;
        };
        t.creating = false;
        t.view = Some(view.clone());
        if !opened_by_page {
            let url = t.url.clone();
            view.navigate(&url);
        }
        if active_id == Some(id) {
            view.bounds(cr);
            view.visible(true);
            if !self.tabs.iter().find(|t| t.id == id).map(|t| t.url.starts_with(NEWTAB)).unwrap_or(false) {
                view.focus();
            }
        } else {
            view.visible(false);
            set_mem(&view, false);
            if let Some(t) = self.tab(id) {
                t.hidden_at = Some(Instant::now());
            }
        }
        self.dirty();
    }

    // ---------------------------------------------------------------- блокировка

    fn check_request(&mut self, id: u64, uri: &str, ctx: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> blocker::Verdict {
        if !self.adblock_on || !(uri.starts_with("http") || uri.starts_with("ws")) {
            return blocker::Verdict::Allow;
        }
        let Some(engine) = &self.engine else { return blocker::Verdict::Allow };
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return blocker::Verdict::Allow };
        if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT && uri == t.nav_url {
            return blocker::Verdict::Allow;
        }
        let src = if t.nav_url.starts_with("http") { t.nav_url.clone() } else { t.url.clone() };
        if self.allow.contains(&host_of(&src)) {
            return blocker::Verdict::Allow;
        }
        let kind = match ctx {
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT => "sub_frame",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET => "stylesheet",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE => "image",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA => "media",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT => "font",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT => "script",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST | COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH | COREWEBVIEW2_WEB_RESOURCE_CONTEXT_EVENT_SOURCE => "xmlhttprequest",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET => "websocket",
            COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING => "ping",
            _ => "other",
        };
        let v = blocker::check(engine, uri, &src, kind);
        if !matches!(v, blocker::Verdict::Allow) {
            t.blocked += 1;
            self.total_blocked += 1;
            self.dirty();
        }
        v
    }

    /// Косметика (скрытие элементов) + скриптлеты: ставятся до загрузки документа.
    fn on_nav_start(&mut self, id: u64, uri: String) {
        let allowed = self.allow.contains(&host_of(&uri));
        let (on, engine_ready) = (self.adblock_on && !allowed, self.engine.is_some());
        let mut script = None;
        let mut ex = vec![];
        if on && engine_ready && uri.starts_with("http") {
            let engine = self.engine.as_ref().unwrap();
            let (css, js, gh, exceptions) = blocker::cosmetic(engine, &uri);
            ex = exceptions;
            let host = host_of(&uri);
            script = Some(format!(
                "(function(){{if(window.top!==window)return;var H={};var h=location.hostname.replace(/^www\\./,'');if(h!==H)return;window.__monoCos={{gh:{}}};try{{var s=new CSSStyleSheet();s.replaceSync({});document.adoptedStyleSheets=[...document.adoptedStyleSheets,s];}}catch(e){{}}try{{{}}}catch(e){{}}}})();",
                wv::js_str(&host),
                gh,
                wv::js_str(&css),
                js,
            ));
        }
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) else { return };
        t.nav_url = uri;
        t.loading = true;
        t.blocked = 0;
        t.cos_ex = ex;
        let Some(view) = t.view.clone() else { return };
        if let Some(old) = t.cos_script.take() {
            unsafe {
                let _ = view.wv.RemoveScriptToExecuteOnDocumentCreated(&h(&old));
            }
        }
        if let Some(js) = script {
            let handler = AddScriptToExecuteOnDocumentCreatedCompletedHandler::create(Box::new(move |res, sid| {
                if res.is_ok() {
                    app(move |a| {
                        if let Some(t) = a.tab(id) {
                            t.cos_script = Some(sid);
                        }
                    });
                }
                Ok(())
            }));
            unsafe {
                let _ = view.wv.AddScriptToExecuteOnDocumentCreated(&h(&js), &handler);
            }
        }
        self.dirty();
    }

    // ---------------------------------------------------------------- всплывающее окно

    fn show_popup(&mut self, x: f64, y: f64, w: f64, hgt: f64) {
        let s = scale(self.hwnd);
        let mut pt = POINT { x: (x * s) as i32, y: (y * s) as i32 + self.top_strip() };
        unsafe {
            let _ = ClientToScreen(self.hwnd, &mut pt);
            let _ = SetWindowPos(self.popup_hwnd, Some(HWND_TOP), pt.x, pt.y, (w * s) as i32, (hgt * s) as i32, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        if let Some(p) = &self.popup {
            p.bounds(RECT { left: 0, top: 0, right: (w * s) as i32, bottom: (hgt * s) as i32 });
            p.visible(true);
        }
        self.popup_on = true;
    }

    fn hide_popup(&mut self) {
        if self.popup_on {
            self.popup_on = false;
            unsafe {
                let _ = ShowWindow(self.popup_hwnd, SW_HIDE);
            }
            if let Some(p) = &self.popup {
                p.post(r#"{"t":"clear"}"#);
            }
        }
    }

    fn send_sug(&mut self, net: &[String]) {
        let q = self.sug_q.clone();
        let mut items: Vec<Value> = vec![];
        let ql = q.to_lowercase();
        let strip = |u: &str| {
            let s = u.split("://").nth(1).unwrap_or(u);
            s.strip_prefix("www.").unwrap_or(s).to_lowercase()
        };
        let mut locals = self.sug_local.clone();
        // лучшее совпадение по адресу — первым (для автодополнения)
        if let Some(first) = locals.first() {
            if strip(first["url"].as_str().unwrap_or("")).starts_with(&ql) {
                items.push(locals.remove(0));
            }
        }
        items.push(json!({"kind": "search", "title": q, "url": self.search_url(&q)}));
        for l in locals.into_iter().take(4) {
            items.push(l);
        }
        for s in net.iter().filter(|s| s.to_lowercase() != ql).take(8usize.saturating_sub(items.len())) {
            items.push(json!({"kind": "search", "title": s, "url": self.search_url(s)}));
        }
        let msg = json!({"t": "sug", "q": q, "items": items}).to_string();
        if let Some(ui) = &self.ui {
            ui.post(&msg);
        }
        if let Some(p) = &self.popup {
            p.post(&msg);
        }
    }

    // ---------------------------------------------------------------- команды

    fn cmd(&mut self, c: &str) {
        let view = self.active_view();
        match c {
            "newtab" => {
                let at = self.tabs.len();
                self.new_tab(NEWTAB, Some(at), true);
            }
            "close" => self.close_tab(self.active),
            "reopen" => {
                if let Some(u) = self.closed.pop() {
                    let at = self.active + 1;
                    self.new_tab(&u, Some(at), true);
                }
            }
            "next" => {
                let n = (self.active + 1) % self.tabs.len().max(1);
                self.select(n)
            }
            "prev" => {
                let n = (self.active + self.tabs.len() - 1) % self.tabs.len().max(1);
                self.select(n)
            }
            "last" => {
                let n = self.tabs.len().saturating_sub(1);
                self.select(n)
            }
            "focusaddr" => self.focus_address(""),
            "back" => unsafe {
                if let Some(v) = view {
                    let _ = v.wv.GoBack();
                }
            },
            "fwd" => unsafe {
                if let Some(v) = view {
                    let _ = v.wv.GoForward();
                }
            },
            "reload" => unsafe {
                if let Some(v) = view {
                    let _ = v.wv.Reload();
                }
            },
            "stop" => unsafe {
                if let Some(v) = view {
                    let _ = v.wv.Stop();
                }
            },
            "bookmark" => {
                if let Some(t) = self.tabs.get(self.active) {
                    if !t.url.starts_with(INTERNAL) && !t.url.is_empty() {
                        if self.store.is_bookmarked(&t.url) {
                            self.store.remove_bookmark_url(&t.url);
                        } else {
                            let title = if t.title.is_empty() { t.url.clone() } else { t.title.clone() };
                            self.store.add_bookmark(&t.url, &title, "");
                        }
                    }
                }
                self.send_bar();
                self.dirty();
            }
            "togglebar" => {
                self.bar_on = !self.bar_on;
                self.store.set("bar", if self.bar_on { "1" } else { "0" });
                self.send_bar();
                self.layout();
            }
            "shield" => {
                if let Some(t) = self.tabs.get(self.active) {
                    let host = host_of(&t.url);
                    if !host.is_empty() {
                        if !self.allow.remove(&host) {
                            self.allow.insert(host);
                        }
                        let list: Vec<&String> = self.allow.iter().collect();
                        self.store.set("allow", &serde_json::to_string(&list).unwrap_or_default());
                        self.cmd("reload");
                    }
                }
                self.dirty();
            }
            "history" | "bookmarks" | "settings" | "passwords" => self.open_internal(c),
            "import" => self.open_internal("settings#import"),
            "downloads" => unsafe {
                if let Some(v) = view {
                    if let Ok(v9) = v.wv.cast::<ICoreWebView2_9>() {
                        let _ = v9.OpenDefaultDownloadDialog();
                    }
                }
            },
            "print" => unsafe {
                if let Some(v) = view {
                    if let Ok(v16) = v.wv.cast::<ICoreWebView2_16>() {
                        let _ = v16.ShowPrintUI(COREWEBVIEW2_PRINT_DIALOG_KIND_BROWSER);
                    }
                }
            },
            "devtools" => unsafe {
                if let Some(v) = view {
                    let _ = v.wv.OpenDevToolsWindow();
                }
            },
            "zoomin" | "zoomout" | "zoomreset" => {
                if let Some(t) = self.tabs.get_mut(self.active) {
                    let steps = [0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
                    let cur = t.zoom;
                    t.zoom = match c {
                        "zoomin" => *steps.iter().find(|s| **s > cur + 0.001).unwrap_or(&3.0),
                        "zoomout" => *steps.iter().rev().find(|s| **s < cur - 0.001).unwrap_or(&0.25),
                        _ => 1.0,
                    };
                    if let Some(v) = &t.view {
                        unsafe {
                            let _ = v.ctl.SetZoomFactor(t.zoom);
                        }
                    }
                }
                self.dirty();
            }
            "fullscreen" => {
                let f = !self.fullscreen;
                self.set_fullscreen(f)
            }
            "quit" => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            _ => {
                if let Some(n) = c.strip_prefix("tab") {
                    if let Ok(n) = n.parse::<usize>() {
                        self.select(n.saturating_sub(1).min(self.tabs.len() - 1));
                    }
                }
            }
        }
    }

    fn open_internal(&mut self, page: &str) {
        let url = format!("{INTERNAL}{page}");
        let base = url.split('#').next().unwrap_or(&url).to_string();
        // уже открыта — переключиться
        if let Some(i) = self.tabs.iter().position(|t| t.url.split('#').next() == Some(&base)) {
            self.select(i);
            if url.contains('#') {
                self.navigate_active(&url);
            }
            return;
        }
        let at = self.active + 1;
        self.new_tab(&url, Some(at), true);
    }

    // ---------------------------------------------------------------- сообщения панели

    fn on_ui(&mut self, m: Value) {
        let t = m["t"].as_str().unwrap_or("");
        match t {
            "dbg" if std::env::var("MONO_TEST").is_ok() => dbg_log(&format!("ui {}", m["v"])),
            "ready" => {
                if let (Ok(js), Some(ui)) = (std::env::var("MONO_UI_EVAL"), &self.ui) {
                    ui.eval(&js);
                }
                self.last_state.clear();
                self.send_bar();
                self.dirty();
                if self.tabs.get(self.active).map(|t| t.url.starts_with(NEWTAB)).unwrap_or(false) {
                    self.focus_address("");
                }
            }
            "nav" => {
                let url = self.resolve(m["text"].as_str().unwrap_or(""));
                self.navigate_active(&url);
            }
            "go" => {
                let url = m["url"].as_str().unwrap_or("").to_string();
                if !url.is_empty() {
                    self.navigate_active(&url);
                }
            }
            "select" => {
                if let Some(i) = m["id"].as_u64().and_then(|id| self.idx(id)) {
                    self.select(i);
                }
            }
            "close" => {
                if let Some(i) = m["id"].as_u64().and_then(|id| self.idx(id)) {
                    self.close_tab(i);
                }
            }
            "mute" => {
                if let Some(t) = m["id"].as_u64().and_then(|id| self.tab(id)) {
                    t.muted = !t.muted;
                    if let Some(v) = &t.view {
                        if let Ok(v8) = v.wv.cast::<ICoreWebView2_8>() {
                            unsafe {
                                let _ = v8.SetIsMuted(t.muted);
                            }
                        }
                    }
                }
                self.dirty();
            }
            "move" => {
                if let (Some(i), Some(to)) = (m["id"].as_u64().and_then(|id| self.idx(id)), m["to"].as_u64()) {
                    let to = (to as usize).min(self.tabs.len() - 1);
                    let active_id = self.tabs[self.active].id;
                    let t = self.tabs.remove(i);
                    self.tabs.insert(to, t);
                    self.active = self.idx(active_id).unwrap_or(0);
                    self.session_dirty = true;
                    self.dirty();
                }
            }
            "drag" => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_DRAG, WPARAM(0), LPARAM(0));
            },
            "min" | "max" => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_WINCMD, WPARAM(if t == "min" { 1 } else { 2 }), LPARAM(0));
            },
            "sug" => {
                let q = m["q"].as_str().unwrap_or("").to_string();
                if q.trim().is_empty() {
                    self.hide_popup();
                    return;
                }
                self.sug_q = q.clone();
                self.sug_seq += 1;
                self.sug_local = self
                    .store
                    .suggest(&q, 6)
                    .into_iter()
                    .map(|e: Entry| json!({"kind": e.kind, "title": e.title, "url": e.url}))
                    .collect();
                self.send_sug(&[]);
                let _ = self.sug_tx.send((self.sug_seq, SEARCH[self.search].sug.replace("{q}", &urlenc(&q))));
                let (x, y, w) = (m["x"].as_f64().unwrap_or(0.0), m["y"].as_f64().unwrap_or(0.0), m["w"].as_f64().unwrap_or(400.0));
                if let Some(p) = &self.popup {
                    p.post(&json!({"t": "geo", "x": x, "y": y, "w": w, "kind": "sug"}).to_string());
                }
            }
            "sugsel" => {
                if let Some(p) = &self.popup {
                    p.post(&json!({"t": "sel", "i": m["i"]}).to_string());
                }
            }
            "hidepop" => self.hide_popup(),
            "opennew" => {
                let url = m["url"].as_str().unwrap_or("").to_string();
                let at = self.active + 1;
                self.new_tab(&url, Some(at), false);
            }
            "folder" => {
                if self.popup_on {
                    self.hide_popup();
                    return;
                }
                let items = self.folder_items(m["name"].as_str().unwrap_or(""));
                if let Some(p) = &self.popup {
                    p.post(&json!({"t": "list", "x": m["x"], "y": m["y"], "items": items}).to_string());
                }
            }
            "barctx" => {
                if let Some(p) = &self.popup {
                    p.post(&json!({"t": "ctx", "x": m["x"], "y": m["y"], "id": m["id"], "url": m["url"]}).to_string());
                }
            }
            "shieldpanel" => {
                if self.popup_on {
                    self.hide_popup();
                    return;
                }
                let (host, blocked) = self
                    .tabs
                    .get(self.active)
                    .map(|t| (if t.url.starts_with("http") { host_of(&t.url) } else { String::new() }, t.blocked))
                    .unwrap_or_default();
                let on = !self.allow.contains(&host);
                if let Some(p) = &self.popup {
                    p.post(&json!({"t": "shield", "x": m["x"], "y": m["y"], "host": host, "on": on, "global": self.adblock_on, "blocked": blocked, "rules": self.rules}).to_string());
                }
            }
            "menu" => {
                if self.popup_on {
                    self.hide_popup();
                    return;
                }
                let zoom = self.tabs.get(self.active).map(|t| t.zoom).unwrap_or(1.0);
                if let Some(p) = &self.popup {
                    p.post(&json!({"t": "menu", "x": m["x"], "y": m["y"], "w": m["w"], "zoom": zoom}).to_string());
                }
            }
            "cmd" => {
                let c = m["c"].as_str().unwrap_or("").to_string();
                self.cmd(&c);
            }
            "focuspage" => {
                if let Some(v) = self.active_view() {
                    v.focus();
                }
            }
            "pw" => {
                if let Some((host, user, pass)) = self.pw_pending.take() {
                    if m["save"].as_bool() == Some(true) {
                        if let Some(b) = vault::protect(&pass) {
                            self.store.save_login(&host, &user, &b);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn on_popup(&mut self, m: Value) {
        if std::env::var("MONO_TEST").is_ok() {
            dbg_log(&format!("popup {m}"));
        }
        match m["t"].as_str().unwrap_or("") {
            "size" => {
                let (x, y, w, hh) = (m["x"].as_f64().unwrap_or(0.0), m["y"].as_f64().unwrap_or(0.0), m["w"].as_f64().unwrap_or(300.0), m["h"].as_f64().unwrap_or(0.0));
                if hh < 1.0 {
                    self.hide_popup();
                } else {
                    self.show_popup(x, y, w, hh);
                }
            }
            "pick" => {
                let url = m["url"].as_str().unwrap_or("").to_string();
                self.hide_popup();
                if let Some(ui) = &self.ui {
                    ui.post(r#"{"t":"blur"}"#);
                }
                if m["newtab"].as_bool() == Some(true) {
                    let at = self.active + 1;
                    self.new_tab(&url, Some(at), true);
                } else {
                    self.navigate_active(&url);
                }
            }
            "cmd" => {
                self.hide_popup();
                let c = m["c"].as_str().unwrap_or("").to_string();
                if let Some(id) = c.strip_prefix("delbm:").and_then(|v| v.parse::<i64>().ok()) {
                    self.store.remove_bookmark(id);
                    self.send_bar();
                    self.dirty();
                    return;
                }
                if let Some(url) = c.strip_prefix("open:") {
                    let at = self.active + 1;
                    self.new_tab(url, Some(at), true);
                    return;
                }
                self.cmd(&c);
                if c.starts_with("zoom") {
                    // меню масштаба остаётся открытым
                    let zoom = self.tabs.get(self.active).map(|t| t.zoom).unwrap_or(1.0);
                    if let Some(p) = &self.popup {
                        p.post(&json!({"t": "menu", "again": true, "zoom": zoom}).to_string());
                    }
                }
            }
            "hide" => self.hide_popup(),
            _ => {}
        }
    }

    // ---------------------------------------------------------------- сообщения страниц

    fn on_page(&mut self, id: u64, source: String, m: Value) {
        let t = m["t"].as_str().unwrap_or("");
        let view = self.tabs.iter().find(|t| t.id == id).and_then(|t| t.view.clone());
        let Some(view) = view else { return };
        match t {
            "bgclick" => self.bg_click = Some(Instant::now()),
            "dbg" if std::env::var("MONO_TEST").is_ok() => dbg_log(&format!("page {}", m["v"])),
            "cos" => {
                // скрипт косметики не успел встать до загрузки — применяем сейчас
                let allowed = self.allow.contains(&host_of(&source));
                if let (true, Some(engine)) = (self.adblock_on && !allowed, &self.engine) {
                    let (css, js, gh, ex) = blocker::cosmetic(engine, &source);
                    if let Some(t) = self.tab(id) {
                        t.cos_ex = ex;
                    }
                    view.eval(&format!(
                        "(function(){{window.__monoCos={{gh:{gh}}};try{{var s=new CSSStyleSheet();s.replaceSync({});document.adoptedStyleSheets=[...document.adoptedStyleSheets,s];}}catch(e){{}}try{{{js}}}catch(e){{}}window.__monoGeneric&&window.__monoGeneric();}})()",
                        wv::js_str(&css)
                    ));
                }
            }
            "gen" => {
                if !self.adblock_on || self.allow.contains(&host_of(&source)) {
                    return;
                }
                let Some(engine) = &self.engine else { return };
                let list = |k: &str| -> Vec<String> {
                    m[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
                };
                let ex = self.tabs.iter().find(|t| t.id == id).map(|t| t.cos_ex.clone()).unwrap_or_default();
                let css = blocker::generic(engine, &list("c"), &list("i"), &ex);
                if !css.is_empty() {
                    view.post(&json!({"t": "gen", "css": css}).to_string());
                }
            }
            "pw-q" => {
                let host = host_of(&source);
                let list: Vec<Value> = self
                    .store
                    .logins_for(&host)
                    .into_iter()
                    .filter_map(|l| vault::unprotect(&l.blob).map(|p| json!({"u": l.username, "p": p})))
                    .collect();
                if !list.is_empty() {
                    view.post(&json!({"t": "pw-a", "list": list}).to_string());
                }
            }
            "pw-save" => {
                let host = host_of(&source);
                let user = m["u"].as_str().unwrap_or("").to_string();
                let pass = m["p"].as_str().unwrap_or("").to_string();
                if host.is_empty() || pass.is_empty() || pass.len() > 512 {
                    return;
                }
                let known = self
                    .store
                    .logins_for(&host)
                    .into_iter()
                    .any(|l| l.username == user && vault::unprotect(&l.blob).as_deref() == Some(pass.as_str()));
                if known || self.pw_pending.as_ref().map(|p| p.0 == host && p.1 == user && p.2 == pass).unwrap_or(false) {
                    return;
                }
                if let Some(ui) = &self.ui {
                    ui.post(&json!({"t": "pw", "host": host, "user": user}).to_string());
                }
                self.pw_pending = Some((host, user, pass));
            }
            "page" if source.starts_with(INTERNAL) => self.on_internal(&view, m),
            _ => {}
        }
    }

    fn on_internal(&mut self, view: &View, m: Value) {
        let op = m["op"].as_str().unwrap_or("").to_string();
        let reply = |data: Value| view.post(&json!({"t": "page", "op": op, "data": data}).to_string());
        match op.as_str() {
            "newtab" => {
                let bms = self.store.bookmarks();
                reply(json!({"top": self.store.top_sites(8), "bookmarks": bms}));
            }
            "history" => {
                let q = m["q"].as_str().unwrap_or("");
                let off = m["offset"].as_i64().unwrap_or(0);
                reply(json!(self.store.history(q, 200, off)));
            }
            "delhist" => {
                self.store.delete_history(m["url"].as_str().unwrap_or(""));
                reply(json!(true));
            }
            "clearhist" => {
                self.store.clear_history();
                reply(json!(true));
            }
            "bookmarks" => reply(json!(self.store.bookmarks())),
            "delbm" => {
                self.store.remove_bookmark(m["id"].as_i64().unwrap_or(0));
                self.send_bar();
                self.dirty();
                reply(json!(self.store.bookmarks()));
            }
            "renamebm" => {
                self.store.rename_bookmark(m["id"].as_i64().unwrap_or(0), m["title"].as_str().unwrap_or(""));
                reply(json!(true));
            }
            "settings" => {
                let ver = self
                    .env
                    .as_ref()
                    .map(|e| get_str(|p| unsafe { e.BrowserVersionString(p) }))
                    .unwrap_or_default();
                reply(json!({
                    "search": SEARCH[self.search].key,
                    "engines": SEARCH.iter().map(|s| json!({"key": s.key, "name": s.name})).collect::<Vec<_>>(),
                    "adblock": self.adblock_on,
                    "rules": self.rules,
                    "blocked": self.total_blocked + self.store.get("blocked_total").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0),
                    "allow": self.allow.iter().collect::<Vec<_>>(),
                    "chrome": import::chrome_profile().is_some(),
                    "bookmarks": self.store.bookmark_count(),
                    "logins": self.store.all_logins().len(),
                    "imported": self.store.get("imported").is_some(),
                    "engine": ver,
                    "version": env!("CARGO_PKG_VERSION"),
                }));
            }
            "set" => {
                let k = m["k"].as_str().unwrap_or("");
                match k {
                    "search" => {
                        let v = m["v"].as_str().unwrap_or("");
                        if let Some(i) = SEARCH.iter().position(|s| s.key == v) {
                            self.search = i;
                            self.store.set("search", v);
                        }
                    }
                    "adblock" => {
                        self.adblock_on = m["v"].as_bool().unwrap_or(true);
                        self.store.set("adblock", if self.adblock_on { "1" } else { "0" });
                        self.dirty();
                    }
                    "unallow" => {
                        self.allow.remove(m["v"].as_str().unwrap_or(""));
                        let list: Vec<&String> = self.allow.iter().collect();
                        self.store.set("allow", &serde_json::to_string(&list).unwrap_or_default());
                    }
                    _ => {}
                }
                reply(json!(true));
            }
            "import" => {
                let what = m["what"].as_str().unwrap_or("").to_string();
                spawn_import(self.data.clone(), what, self.tx.clone());
                reply(json!("started"));
            }
            "importpw" => {
                let r = import::passwords_csv(&self.store, m["csv"].as_str().unwrap_or(""));
                reply(match r {
                    Ok(n) => json!({"ok": n}),
                    Err(e) => json!({"err": e}),
                });
            }
            "logins" => {
                let list: Vec<Value> = self.store.all_logins().into_iter().map(|l| json!({"id": l.id, "host": l.host, "user": l.username})).collect();
                reply(json!(list));
            }
            "reveal" => {
                let p = self.store.login_blob(m["id"].as_i64().unwrap_or(0)).and_then(|b| vault::unprotect(&b));
                reply(json!({"id": m["id"], "p": p}));
            }
            "dellogin" => {
                self.store.delete_login(m["id"].as_i64().unwrap_or(0));
                reply(json!(true));
            }
            "updatelists" => {
                let tx = self.tx.clone();
                let d = self.data.clone();
                std::thread::spawn(move || {
                    let r = blocker::load(&d, true);
                    let _ = tx.send(Msg::Engine(r));
                    let _ = tx.send(Msg::Note("lists".into()));
                    wake();
                });
                reply(json!("started"));
            }
            "clear" => {
                let kinds = match m["what"].as_str().unwrap_or("") {
                    "cache" => COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE | COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE,
                    "cookies" => COREWEBVIEW2_BROWSING_DATA_KINDS_COOKIES | COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_DOM_STORAGE,
                    _ => COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_PROFILE,
                };
                if let Ok(p) = view.wv.cast::<ICoreWebView2_13>().and_then(|v| unsafe { v.Profile() }) {
                    if let Ok(p2) = p.cast::<ICoreWebView2Profile2>() {
                        unsafe {
                            let _ = p2.ClearBrowsingData(kinds, None);
                        }
                    }
                }
                reply(json!(true));
            }
            "open" => {
                let url = m["url"].as_str().unwrap_or("").to_string();
                if m["newtab"].as_bool() == Some(true) {
                    let at = self.active + 1;
                    self.new_tab(&url, Some(at), false);
                } else {
                    self.navigate_active(&url);
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- фон

    fn on_msgs(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Engine(Some(l)) => {
                    self.rules = l.rules;
                    self.engine = Some(l.engine);
                }
                Msg::Engine(None) => {}
                Msg::Suggest(seq, list) => {
                    if seq == self.sug_seq {
                        self.send_sug(&list);
                    }
                }
                Msg::Note(s) => {
                    self.send_bar();
                    for t in &self.tabs {
                        if t.url.starts_with(INTERNAL) {
                            if let Some(v) = &t.view {
                                v.post(&json!({"t": "page", "op": "note", "data": s}).to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    fn tick(&mut self) {
        // «Спящие» вкладки: фоновые > 10 минут без звука
        for t in self.tabs.iter_mut() {
            if let (Some(at), Some(v), false, false) = (t.hidden_at, &t.view, t.suspended, t.audio) {
                if at.elapsed() > Duration::from_secs(600) {
                    if let Ok(v3) = v.wv.cast::<ICoreWebView2_3>() {
                        unsafe {
                            let _ = v3.TrySuspend(&TrySuspendCompletedHandler::create(Box::new(|_, _| Ok(()))));
                        }
                    }
                    t.suspended = true;
                }
            }
        }
        if self.session_dirty {
            self.save_session();
        }
        self.persist_blocked();
    }

    fn persist_blocked(&mut self) {
        if self.total_blocked > 0 {
            let prev = self.store.get("blocked_total").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            self.store.set("blocked_total", &(prev + self.total_blocked).to_string());
            self.total_blocked = 0;
        }
    }

    fn save_session(&mut self) {
        self.session_dirty = false;
        let urls: Vec<&String> = self.tabs.iter().map(|t| &t.url).collect();
        let s = json!({"tabs": urls, "active": self.active}).to_string();
        self.store.set("session", &s);
        unsafe {
            let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
            if !self.fullscreen && GetWindowPlacement(self.hwnd, &mut wp).is_ok() {
                let r = wp.rcNormalPosition;
                self.store.set("window", &format!("{},{},{},{},{}", r.left, r.top, r.right, r.bottom, (wp.showCmd == SW_SHOWMAXIMIZED.0 as u32) as i32));
            }
        }
    }
}

fn dbg_log(line: &str) {
    if let Ok(d) = std::env::var("MONO_DATA") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(PathBuf::from(d).join("dbg.log")) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn display_title(url: &str) -> String {
    if url.starts_with(NEWTAB) {
        return "Новая вкладка".into();
    }
    if url.starts_with(INTERNAL) {
        let p = url.trim_start_matches(INTERNAL).split('#').next().unwrap_or("");
        return match p {
            "history" => "История",
            "bookmarks" => "Закладки",
            "passwords" => "Пароли",
            _ => "Настройки",
        }
        .into();
    }
    let h = host_of(url);
    if h.is_empty() { url.into() } else { h }
}

fn set_mem(v: &View, normal: bool) {
    if let Ok(v19) = v.wv.cast::<ICoreWebView2_19>() {
        unsafe {
            let _ = v19.SetMemoryUsageTargetLevel(if normal { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL } else { COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW });
        }
    }
}

fn spawn_import(data: PathBuf, what: String, tx: mpsc::Sender<Msg>) {
    std::thread::spawn(move || {
        let Ok(st) = Store::open(&data.join("mono.db")) else { return };
        let mut notes = vec![];
        if what == "bookmarks" || what == "all" {
            notes.push(match import::bookmarks(&st) {
                Ok(n) => format!("Закладки: перенесено {n}"),
                Err(e) => format!("Закладки: {e}"),
            });
        }
        if what == "history" || what == "all" {
            notes.push(match import::history(&st) {
                Ok(n) => format!("История: перенесено {n}"),
                Err(e) => format!("История: {e}"),
            });
        }
        st.set("imported", "1");
        let _ = tx.send(Msg::Note(notes.join(" · ")));
        wake();
    });
}

// ---------------------------------------------------------------- настройка WebView

fn settings_common(wv: &ICoreWebView2, ui: bool) {
    unsafe {
        if let Ok(s) = wv.Settings() {
            let _ = s.SetIsStatusBarEnabled(!ui);
            let _ = s.SetAreDefaultContextMenusEnabled(!ui);
            let _ = s.SetIsZoomControlEnabled(!ui);
            let _ = s.SetAreDevToolsEnabled(!ui || cfg!(debug_assertions));
            if let Ok(s3) = s.cast::<ICoreWebView2Settings3>() {
                let _ = s3.SetAreBrowserAcceleratorKeysEnabled(!ui);
            }
            if let Ok(s4) = s.cast::<ICoreWebView2Settings4>() {
                // свой менеджер паролей вместо встроенного
                let _ = s4.SetIsPasswordAutosaveEnabled(false);
                let _ = s4.SetIsGeneralAutofillEnabled(!ui);
            }
            if let Ok(s6) = s.cast::<ICoreWebView2Settings6>() {
                let _ = s6.SetIsSwipeNavigationEnabled(!ui);
            }
            if let Ok(s8) = s.cast::<ICoreWebView2Settings8>() {
                // SmartScreen отправляет каждый адрес в Microsoft и тормозит переходы
                let _ = s8.SetIsReputationCheckingRequired(false);
            }
        }
    }
}

fn add_accel(ctl: &ICoreWebView2Controller) {
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(|_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
        let mut vk = 0u32;
        unsafe {
            args.KeyEventKind(&mut kind)?;
            args.VirtualKey(&mut vk)?;
        }
        if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN {
            return Ok(());
        }
        let down = |k: VIRTUAL_KEY| unsafe { GetKeyState(k.0 as i32) < 0 };
        let (ctrl, shift, alt) = (down(VK_CONTROL), down(VK_SHIFT), down(VK_MENU));
        let c: Option<String> = match (ctrl, shift, alt, vk) {
            (true, false, false, 0x54) | (true, false, false, 0x4E) => Some("newtab".into()),
            (true, true, false, 0x54) => Some("reopen".into()),
            (true, false, false, 0x57) | (true, false, false, 0x73) => Some("close".into()),
            (true, false, false, 0x09) | (true, false, false, 0x22) => Some("next".into()),
            (true, true, false, 0x09) | (true, false, false, 0x21) => Some("prev".into()),
            (true, false, false, 0x31..=0x38) => Some(format!("tab{}", vk - 0x30)),
            (true, false, false, 0x39) => Some("last".into()),
            (true, false, false, 0x4C) | (false, false, true, 0x44) | (false, false, false, 0x75) => Some("focusaddr".into()),
            (true, false, false, 0x52) | (false, false, false, 0x74) => Some("reload".into()),
            (true, false, false, 0x44) => Some("bookmark".into()),
            (true, false, false, 0x48) => Some("history".into()),
            (true, true, false, 0x4F) => Some("bookmarks".into()),
            (true, true, false, 0x42) => Some("togglebar".into()),
            (true, false, false, 0x4A) => Some("downloads".into()),
            (false, false, false, 0x7A) => Some("fullscreen".into()),
            (false, false, true, 0x25) => Some("back".into()),
            (false, false, true, 0x27) => Some("fwd".into()),
            (false, false, false, 0x7B) => Some("devtools".into()),
            _ => None,
        };
        if let Some(c) = c {
            unsafe {
                args.SetHandled(true)?;
            }
            app(move |a| a.cmd(&c));
        }
        Ok(())
    }));
    let mut tok = 0i64;
    unsafe {
        let _ = ctl.add_AcceleratorKeyPressed(&handler, &mut tok);
    }
}

fn setup_content(view: &View, id: u64) {
    let wv = &view.wv;
    settings_common(wv, false);
    add_accel(&view.ctl);
    let mut tok = 0i64;
    unsafe {
        let _ = wv.AddScriptToExecuteOnDocumentCreated(&h(CONTENT_JS), None);
        // YouTube — всегда и сразу при создании вкладки, без гонки с началом перехода и загрузкой движка
        if std::env::var("MONO_NOFILTER").is_err() {
            let _ = wv.AddScriptToExecuteOnDocumentCreated(&h(YT_JS), None);
        }
        if let (Ok(js), true) = (std::env::var("MONO_EVAL"), std::env::var("MONO_TEST").is_ok()) {
            let _ = wv.AddScriptToExecuteOnDocumentCreated(&h(&js), None);
        }
        let _ = wv.AddWebResourceRequestedFilter(&h("https://mono.local/*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL);
        if std::env::var("MONO_NOFILTER").is_ok() {
        } else { match wv.cast::<ICoreWebView2_22>() {
            Ok(w22) => {
                let _ = w22.AddWebResourceRequestedFilterWithRequestSourceKinds(&h("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_REQUEST_SOURCE_KINDS_ALL);
            }
            Err(_) => {
                let _ = wv.AddWebResourceRequestedFilter(&h("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL);
            }
        } }

        let _ = wv.add_WebResourceRequested(
            &WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let req = args.Request()?;
                let uri = get_str(|p| req.Uri(p));
                if let Some(path) = uri.strip_prefix(INTERNAL) {
                    let page = path.split(['#', '?']).next().unwrap_or("");
                    let body = if page == "newtab" { NEWTAB_HTML } else { PAGE_HTML };
                    if let Some(env) = try_app(|a| a.env.clone()).flatten() {
                        let stream = SHCreateMemStream(Some(body.as_bytes()));
                        let resp = env.CreateWebResourceResponse(stream.as_ref(), 200, &h("OK"), &h("Content-Type: text/html; charset=utf-8\r\nCache-Control: no-store"))?;
                        args.SetResponse(&resp)?;
                    }
                    return Ok(());
                }
                let mut ctx = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
                args.ResourceContext(&mut ctx)?;
                let Some((verdict, env)) = try_app(|a| (a.check_request(id, &uri, ctx), a.env.clone())) else { return Ok(()) };
                let Some(env) = env else { return Ok(()) };
                match verdict {
                    blocker::Verdict::Allow => {}
                    blocker::Verdict::Block => {
                        let resp = env.CreateWebResourceResponse(None, 403, &h("Blocked"), &h(""))?;
                        args.SetResponse(&resp)?;
                    }
                    blocker::Verdict::Redirect(data) => {
                        // data:mime;base64,....  — отдаём «пустышку», чтобы страница не ломалась
                        let (head, b64) = data.split_once(',').unwrap_or(("", ""));
                        let mime = head.trim_start_matches("data:").split(';').next().unwrap_or("text/plain");
                        use base64::Engine as _;
                        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap_or_default();
                        let stream = SHCreateMemStream(Some(&bytes));
                        let resp = env.CreateWebResourceResponse(stream.as_ref(), 200, &h("OK"), &h(&format!("Content-Type: {mime}")))?;
                        args.SetResponse(&resp)?;
                    }
                }
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let uri = get_str(|p| args.Uri(p));
                if std::env::var("MONO_TEST").is_ok() {
                    NAV_T0.with(|t| t.set(Some(Instant::now())));
                }
                app(move |a| a.on_nav_start(id, uri));
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |wv, _| {
                let Some(wv) = wv else { return Ok(()) };
                let src = get_str(|p| wv.Source(p));
                app(move |a| {
                    if let Some(t) = a.tab(id) {
                        t.url = src;
                    }
                    a.session_dirty = true;
                    a.dirty();
                });
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_HistoryChanged(
            &HistoryChangedEventHandler::create(Box::new(move |_, _| {
                app(|a| a.dirty());
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_DocumentTitleChanged(
            &DocumentTitleChangedEventHandler::create(Box::new(move |wv, _| {
                let Some(wv) = wv else { return Ok(()) };
                let title = get_str(|p| wv.DocumentTitle(p));
                app(move |a| {
                    let mut url = None;
                    if let Some(t) = a.tab(id) {
                        t.title = title.clone();
                        url = Some(t.url.clone());
                    }
                    if let Some(u) = url {
                        if u.starts_with("http") {
                            a.store.set_title(&u, &title);
                        }
                    }
                    a.dirty();
                });
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |wv, args| {
                let ok = args
                    .map(|a| {
                        let mut b = BOOL(0);
                        let _ = a.IsSuccess(&mut b);
                        b.as_bool()
                    })
                    .unwrap_or(false);
                let (src, title) = wv.map(|w| (get_str(|p| w.Source(p)), get_str(|p| w.DocumentTitle(p)))).unwrap_or_default();
                if let Some(t0) = NAV_T0.with(|t| t.take()) {
                    if let Ok(d) = std::env::var("MONO_DATA") {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(PathBuf::from(d).join("nav.log")) {
                            let _ = writeln!(f, "{} {}", t0.elapsed().as_millis(), src);
                        }
                    }
                }
                app(move |a| {
                    if let Some(t) = a.tab(id) {
                        t.loading = false;
                        t.url = src.clone();
                    }
                    if ok && src.starts_with("http") {
                        a.store.visit(&src, &title);
                    }
                    a.dirty();
                });
                Ok(())
            })),
            &mut tok,
        );

        if let Ok(w15) = wv.cast::<ICoreWebView2_15>() {
            let _ = w15.add_FaviconChanged(
                &FaviconChangedEventHandler::create(Box::new(move |wv, _| {
                    let Some(wv) = wv else { return Ok(()) };
                    let fav = wv.cast::<ICoreWebView2_15>().map(|w| get_str(|p| w.FaviconUri(p))).unwrap_or_default();
                    app(move |a| {
                        if let Some(t) = a.tab(id) {
                            t.fav = fav;
                        }
                        a.dirty();
                    });
                    Ok(())
                })),
                &mut tok,
            );
        }

        if let Ok(w8) = wv.cast::<ICoreWebView2_8>() {
            let _ = w8.add_IsDocumentPlayingAudioChanged(
                &IsDocumentPlayingAudioChangedEventHandler::create(Box::new(move |wv, _| {
                    let Some(wv) = wv else { return Ok(()) };
                    let mut b = BOOL(0);
                    if let Ok(w8) = wv.cast::<ICoreWebView2_8>() {
                        let _ = w8.IsDocumentPlayingAudio(&mut b);
                    }
                    let playing = b.as_bool();
                    app(move |a| {
                        if let Some(t) = a.tab(id) {
                            t.audio = playing;
                        }
                        a.dirty();
                    });
                    Ok(())
                })),
                &mut tok,
            );
        }

        let _ = wv.add_ContainsFullScreenElementChanged(
            &ContainsFullScreenElementChangedEventHandler::create(Box::new(move |wv, _| {
                let Some(wv) = wv else { return Ok(()) };
                let mut b = BOOL(0);
                let _ = wv.ContainsFullScreenElement(&mut b);
                let on = b.as_bool();
                app(move |a| a.set_fullscreen(on));
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_WindowCloseRequested(
            &WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
                app(move |a| {
                    if let Some(i) = a.idx(id) {
                        a.close_tab(i);
                    }
                });
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let uri = get_str(|p| args.Uri(p));
                let deferral = args.GetDeferral()?;
                app(move |a| {
                    let bg = a.bg_click.take().map(|t| t.elapsed() < Duration::from_millis(1500)).unwrap_or(false)
                        || GetKeyState(VK_CONTROL.0 as i32) < 0;
                    let at = a.idx(id).map(|i| i + 1).unwrap_or(a.tabs.len());
                    let nid = a.next_id;
                    a.next_id += 1;
                    let pos = at.min(a.tabs.len());
                    a.tabs.insert(pos, Tab::new(nid, &uri));
                    if pos <= a.active {
                        a.active += 1;
                    }
                    let args2 = args.clone();
                    a.create_view(
                        nid,
                        Some(Box::new(move |v: &View| {
                            let _ = args2.SetNewWindow(&v.wv);
                            let _ = args2.SetHandled(true);
                            let _ = deferral.Complete();
                        })),
                    );
                    if !bg {
                        a.select(pos);
                    }
                    a.session_dirty = true;
                    a.dirty();
                });
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let src = get_str(|p| args.Source(p));
                let json = get_str(|p| args.WebMessageAsJson(p));
                if let Ok(m) = serde_json::from_str::<Value>(&json) {
                    app(move |a| a.on_page(id, src, m));
                }
                Ok(())
            })),
            &mut tok,
        );

        let _ = view.ctl.add_GotFocus(
            &FocusChangedEventHandler::create(Box::new(move |_, _| {
                app(|a| {
                    a.hide_popup();
                    if let Some(ui) = &a.ui {
                        ui.post(r#"{"t":"blur"}"#);
                    }
                });
                Ok(())
            })),
            &mut tok,
        );

        let _ = wv.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, _| {
                app(move |a| {
                    if let Some(t) = a.tab(id) {
                        t.loading = false;
                        t.title = "Вкладка упала — обновите (F5)".into();
                    }
                    a.dirty();
                });
                Ok(())
            })),
            &mut tok,
        );
    }
}

/// WebView для панели и выпадашки: только наши сообщения.
fn create_chrome_view(env: &ICoreWebView2Environment, parent: HWND, html: &'static str, is_popup: bool) {
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |res, ctl| {
        if std::env::var("MONO_TEST").is_ok() {
            dbg_log(&format!("chrome view popup={is_popup} res={res:?} ctl={}", ctl.is_some()));
        }
        res?;
        let Some(ctl) = ctl else { return Ok(()) };
        let wv = unsafe { ctl.CoreWebView2()? };
        let view = View { ctl, wv };
        settings_common(&view.wv, true);
        add_accel(&view.ctl);
        view.set_bg(if is_popup { 22 } else { 10 }, if is_popup { 22 } else { 10 }, if is_popup { 22 } else { 10 });
        let mut tok = 0i64;
        unsafe {
            let _ = view.wv.add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let json = get_str(|p| args.WebMessageAsJson(p));
                    if let Ok(m) = serde_json::from_str::<Value>(&json) {
                        app(move |a| if is_popup { a.on_popup(m) } else { a.on_ui(m) });
                    }
                    Ok(())
                })),
                &mut tok,
            );
            let _ = view.wv.NavigateToString(&h(html));
        }
        app(move |a| {
            if is_popup {
                view.visible(false);
                a.popup = Some(view);
            } else {
                a.ui = Some(view);
                a.layout();
            }
        });
        Ok(())
    }));
    unsafe {
        let _ = env.CreateCoreWebView2Controller(parent, &handler);
    }
}

// ---------------------------------------------------------------- оконная процедура

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCALCSIZE if wp.0 != 0 => {
                let full = try_app(|a| a.fullscreen).unwrap_or(false);
                let p = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
                let top = p.rgrc[0].top;
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                if !full {
                    // убираем системный заголовок, оставляя рамки для изменения размера
                    p.rgrc[0].top = top;
                    if IsZoomed(hwnd).as_bool() {
                        let dpi = GetDpiForWindow(hwnd);
                        p.rgrc[0].top += GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
                    }
                }
                r
            }
            WM_NCHITTEST => {
                let r = DefWindowProcW(hwnd, msg, wp, lp);
                if r.0 as u32 == HTCLIENT && !IsZoomed(hwnd).as_bool() {
                    let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
                    let _ = ScreenToClient(hwnd, &mut pt);
                    let strip = (STRIP_DIP * scale(hwnd)).round() as i32;
                    if pt.y < strip {
                        return LRESULT(HTTOP as isize);
                    }
                }
                r
            }
            WM_SIZE => {
                app(|a| a.layout());
                LRESULT(0)
            }
            WM_MOVE | WM_MOVING => {
                app(|a| {
                    a.hide_popup();
                    for t in &a.tabs {
                        if let Some(v) = &t.view {
                            let _ = v.ctl.NotifyParentWindowPositionChanged();
                        }
                    }
                    if let Some(u) = &a.ui {
                        let _ = u.ctl.NotifyParentWindowPositionChanged();
                    }
                });
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let other = lp.0;
                    app(move |a| {
                        if other != a.popup_hwnd.0 as isize {
                            a.hide_popup();
                        }
                    });
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_GETMINMAXINFO => {
                let mi = &mut *(lp.0 as *mut MINMAXINFO);
                let s = scale(hwnd);
                mi.ptMinTrackSize = POINT { x: (480.0 * s) as i32, y: (320.0 * s) as i32 };
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_TIMER => {
                match wp.0 {
                    T_FLUSH => app(|a| a.flush()),
                    T_TICK => app(|a| a.tick()),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_WAKE => {
                drain_queue();
                app(|a| a.on_msgs());
                LRESULT(0)
            }
            WM_DRAG => {
                let _ = ReleaseCapture();
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = SendMessageW(hwnd, WM_NCLBUTTONDOWN, Some(WPARAM(HTCAPTION as usize)), Some(LPARAM(((pt.y as isize) << 16) | (pt.x as isize & 0xFFFF))));
                LRESULT(0)
            }
            WM_WINCMD => {
                match wp.0 {
                    1 => {
                        let _ = ShowWindow(hwnd, SW_MINIMIZE);
                    }
                    _ => {
                        let _ = ShowWindow(hwnd, if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
                    }
                }
                LRESULT(0)
            }
            WM_ERASEBKGND => {
                let dc = HDC(wp.0 as _);
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                let b = CreateSolidBrush(COLORREF(0x000A0A0A));
                FillRect(dc, &r, b);
                let _ = DeleteObject(b.into());
                LRESULT(1)
            }
            WM_COPYDATA => {
                let cds = &*(lp.0 as *const COPYDATASTRUCT);
                if cds.dwData == 0x4D4F4E4F {
                    let s = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
                    let url = String::from_utf16_lossy(s);
                    app(move |a| {
                        let target = if url.is_empty() { NEWTAB.to_string() } else { a.resolve(&url) };
                        let at = a.tabs.len();
                        a.new_tab(&target, Some(at), true);
                    });
                    if IsIconic(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(hwnd);
                }
                LRESULT(1)
            }
            WM_CLOSE => {
                app(|a| {
                    a.save_session();
                    a.persist_blocked();
                });
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---------------------------------------------------------------- запуск

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("MONO_DATA") {
        let d = PathBuf::from(d);
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    let base = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let d = base.join("Mono");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn suggest_worker(rx: mpsc::Receiver<(u64, String)>, tx: mpsc::Sender<Msg>) {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_millis(1500))).build().into();
    while let Ok(mut job) = rx.recv() {
        // берём только самый свежий запрос
        while let Ok(j) = rx.try_recv() {
            job = j;
        }
        let (seq, url) = job;
        let list = agent
            .get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Mono/0.1")
            .call()
            .ok()
            .and_then(|mut r| r.body_mut().read_to_string().ok())
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get(1).and_then(|a| a.as_array().cloned()))
            .map(|a| a.into_iter().filter_map(|x| x.as_str().map(String::from)).take(6).collect::<Vec<_>>())
            .unwrap_or_default();
        let _ = tx.send(Msg::Suggest(seq, list));
        wake();
    }
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // Ссылка из командной строки (так Windows открывает ссылки в браузере по умолчанию)
    let arg_url: Option<String> = std::env::args().skip(1).find(|a| !a.starts_with("--")).map(|a| {
        let p = std::path::Path::new(&a);
        if p.is_absolute() && p.exists() { format!("file:///{}", a.replace('\\', "/")) } else { a }
    });
    // Один экземпляр: если Mono уже открыт — отдаём ему ссылку и выходим
    if std::env::var("MONO_DATA").is_err() {
        unsafe {
            if let Ok(other) = FindWindowW(w!("MonoBrowser"), PCWSTR::null()) {
                if !other.is_invalid() {
                    let payload: Vec<u16> = arg_url.clone().unwrap_or_default().encode_utf16().collect();
                    let cds = COPYDATASTRUCT { dwData: 0x4D4F4E4F, cbData: (payload.len() * 2) as u32, lpData: payload.as_ptr() as _ };
                    let _ = SendMessageW(other, WM_COPYDATA, Some(WPARAM(0)), Some(LPARAM(&cds as *const _ as isize)));
                    return;
                }
            }
        }
    }
    let data = data_dir();
    let store = match Store::open(&data.join("mono.db")) {
        Ok(s) => s,
        Err(_) => return,
    };
    let first_run = store.get("imported").is_none() && store.get("session").is_none();

    let hinst = unsafe { GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default() };
    let icon = unsafe { LoadIconW(Some(hinst), PCWSTR(1 as _)).unwrap_or_default() };
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("MonoBrowser"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let pc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(popup_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("MonoPopup"),
            ..Default::default()
        };
        RegisterClassExW(&pc);
    }

    // Положение окна с прошлого раза
    let (mut x, mut y, mut wdt, mut hgt, mut maxed) = (CW_USEDEFAULT, CW_USEDEFAULT, 1280, 820, false);
    if let Some(s) = store.get("window") {
        let v: Vec<i32> = s.split(',').filter_map(|p| p.parse().ok()).collect();
        if v.len() == 5 && v[2] - v[0] > 200 && v[3] - v[1] > 150 {
            (x, y, wdt, hgt, maxed) = (v[0], v[1], v[2] - v[0], v[3] - v[1], v[4] == 1);
        }
    }
    let hwnd = unsafe {
        CreateWindowExW(WINDOW_EX_STYLE(0), w!("MonoBrowser"), w!("Mono"), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, x, y, wdt, hgt, None, None, Some(hinst), None)
    };
    let Ok(hwnd) = hwnd else { return };
    MAIN_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    unsafe {
        // пересчитать рамку: системный заголовок убирается в WM_NCCALCSIZE
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    let popup_hwnd = unsafe {
        CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, w!("MonoPopup"), w!(""), WS_POPUP | WS_CLIPCHILDREN, 0, 0, 10, 10, Some(hwnd), None, Some(hinst), None)
            .unwrap_or_default()
    };
    unsafe {
        let dark: BOOL = BOOL(1);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let border: u32 = 0x00262626;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as _, 4);
        let m = MARGINS { cxLeftWidth: 0, cxRightWidth: 0, cyTopHeight: 1, cyBottomHeight: 0 };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(popup_hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
        let _ = DwmSetWindowAttribute(popup_hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let pb: u32 = 0x00303030;
        let _ = DwmSetWindowAttribute(popup_hwnd, DWMWA_BORDER_COLOR, &pb as *const _ as _, 4);
    }

    let (tx, rx) = mpsc::channel();
    let (sug_tx, sug_rx) = mpsc::channel();
    {
        let tx = tx.clone();
        std::thread::spawn(move || suggest_worker(sug_rx, tx));
    }
    {
        let tx = tx.clone();
        let d = data.clone();
        std::thread::spawn(move || {
            let r = blocker::load(&d, false);
            let _ = tx.send(Msg::Engine(r));
            wake();
        });
    }
    if first_run && import::chrome_profile().is_some() {
        spawn_import(data.clone(), "all".into(), tx.clone());
    }

    let search = store.get("search").and_then(|k| SEARCH.iter().position(|s| s.key == k)).unwrap_or(0);
    let adblock_on = store.get("adblock").map(|v| v != "0").unwrap_or(true);
    let bar_on = store.get("bar").map(|v| v != "0").unwrap_or(true);
    let bar_n = store.bookmarks().len();
    let allow: HashSet<String> = store.get("allow").and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();

    // Сессия: вкладки восстанавливаются «лениво» — грузится только активная
    let mut tabs = vec![];
    let mut active = 0;
    if let Some(s) = store.get("session").and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
        if let Some(arr) = s["tabs"].as_array() {
            for (i, u) in arr.iter().filter_map(|u| u.as_str()).enumerate() {
                let mut t = Tab::new(i as u64 + 1, u);
                t.title = String::new();
                tabs.push(t);
            }
        }
        active = s["active"].as_u64().unwrap_or(0) as usize;
    }
    if first_run {
        tabs.push(Tab::new(tabs.len() as u64 + 1, &format!("{INTERNAL}settings#welcome")));
        active = tabs.len() - 1;
    }
    if let Some(u) = &arg_url {
        tabs.push(Tab::new(tabs.len() as u64 + 1, u));
        active = tabs.len() - 1;
    }
    if tabs.is_empty() {
        tabs.push(Tab::new(1, NEWTAB));
    }
    if active >= tabs.len() {
        active = 0;
    }
    let next_id = tabs.len() as u64 + 1;

    APP.with(|c| {
        *c.borrow_mut() = Some(App {
            hwnd,
            popup_hwnd,
            env: None,
            ui: None,
            popup: None,
            popup_on: false,
            tabs,
            active,
            next_id,
            closed: vec![],
            store,
            data: data.clone(),
            engine: None,
            rules: 0,
            total_blocked: 0,
            adblock_on,
            allow,
            search,
            tx,
            rx,
            sug_tx,
            sug_seq: 0,
            sug_q: String::new(),
            sug_local: vec![],
            fullscreen: false,
            saved_style: 0,
            saved_rect: RECT::default(),
            dirty: false,
            session_dirty: false,
            bg_click: None,
            pw_pending: None,
            last_state: String::new(),
            bar_on,
            bar_n,
        })
    });

    let test = std::env::var("MONO_TEST").is_ok();
    unsafe {
        if test {
            // тестовый прогон: окно за экраном и без фокуса
            let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), -3000, 0, 1280, 820, SWP_NOACTIVATE);
        }
        let _ = ShowWindow(hwnd, if test { SW_SHOWNOACTIVATE } else if maxed { SW_SHOWMAXIMIZED } else { SW_SHOW });
        let _ = UpdateWindow(hwnd);
        SetTimer(Some(hwnd), T_TICK, 60_000, None);
    }

    // Окружение WebView2: свой профиль, флаги ускорения
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe {
        let args = std::env::var("MONO_ARGS").unwrap_or_else(|_| {
            "--enable-features=ParallelDownloading --disable-features=msSmartScreenProtection".into()
        });
        options.set_additional_browser_arguments(args);
        options.set_language("ru-RU".into());
        // трекеры режет наш блокировщик — встроенная защита только удвоит работу
        options.set_enable_tracking_prevention(false);
    }
    let options: ICoreWebView2EnvironmentOptions = options.into();
    let env_handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |res, env| {
        if res.is_err() {
            unsafe {
                MessageBoxW(None, w!("Не найден WebView2 Runtime. Установите его с сайта Microsoft."), w!("Mono"), MB_ICONERROR);
                PostQuitMessage(1);
            }
            return Ok(());
        }
        let Some(env) = env else { return Ok(()) };
        app(move |a| {
            a.env = Some(env.clone());
            create_chrome_view(&env, a.hwnd, UI_HTML, false);
            create_chrome_view(&env, a.popup_hwnd, POPUP_HTML, true);
            let id = a.tabs[a.active].id;
            a.create_view(id, None);
        });
        Ok(())
    }));
    let udf = data.join("profile");
    unsafe {
        let _ = CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &h(&udf.to_string_lossy()), &options, &env_handler);
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
