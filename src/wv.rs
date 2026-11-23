//! Мелкие помощники поверх COM WebView2.

use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::take_pwstr;
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::RECT;

/// Достаёт строку из COM-геттера вида `fn X(&self, out: *mut PWSTR)`.
pub fn get_str(f: impl FnOnce(*mut PWSTR) -> windows::core::Result<()>) -> String {
    let mut p = PWSTR::null();
    if f(&mut p).is_ok() && !p.is_null() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

pub fn h(s: &str) -> HSTRING {
    HSTRING::from(s)
}

#[derive(Clone)]
pub struct View {
    pub ctl: ICoreWebView2Controller,
    pub wv: ICoreWebView2,
}

impl View {
    pub fn post(&self, json: &str) {
        unsafe {
            let _ = self.wv.PostWebMessageAsJson(&h(json));
        }
    }
    pub fn navigate(&self, url: &str) {
        unsafe {
            let _ = self.wv.Navigate(&h(url));
        }
    }
    pub fn bounds(&self, r: RECT) {
        unsafe {
            let _ = self.ctl.SetBounds(r);
        }
    }
    pub fn visible(&self, v: bool) {
        unsafe {
            let _ = self.ctl.SetIsVisible(v);
        }
    }
    pub fn focus(&self) {
        unsafe {
            let _ = self.ctl.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
        }
    }
    pub fn eval(&self, js: &str) {
        unsafe {
            let _ = self.wv.ExecuteScript(&h(js), None);
        }
    }
    pub fn can_back(&self) -> bool {
        let mut b = windows::core::BOOL(0);
        unsafe {
            let _ = self.wv.CanGoBack(&mut b);
        }
        b.as_bool()
    }
    pub fn can_fwd(&self) -> bool {
        let mut b = windows::core::BOOL(0);
        unsafe {
            let _ = self.wv.CanGoForward(&mut b);
        }
        b.as_bool()
    }
    pub fn close(&self) {
        unsafe {
            let _ = self.ctl.Close();
        }
    }
    pub fn set_bg(&self, r: u8, g: u8, b: u8) {
        use windows::core::Interface;
        if let Ok(c2) = self.ctl.cast::<ICoreWebView2Controller2>() {
            unsafe {
                let _ = c2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 255, R: r, G: g, B: b });
            }
        }
    }
}

/// JSON-строка с экранированием (для вставки в JS / сообщения).
pub fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}
