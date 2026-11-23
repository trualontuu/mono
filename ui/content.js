// Встраивается в каждую страницу до её скриптов.
(function () {
  if (window.__mono) return;
  window.__mono = 1;
  var W = window.chrome && window.chrome.webview;
  if (!W) return;
  var post = function (m) { try { W.postMessage(m); } catch (e) {} };

  // Колёсико / Ctrl+клик по ссылке — открыть вкладку в фоне
  addEventListener('mousedown', function (e) {
    if ((e.button === 1 || e.ctrlKey) && e.target && e.target.closest && e.target.closest('a[href]')) post({ t: 'bgclick' });
  }, true);

  if (window.top !== window) return;
  if (location.protocol !== 'http:' && location.protocol !== 'https:') return;
  if (location.origin === 'https://mono.local') return;

  function addCss(css) {
    if (!css) return;
    try {
      var s = new CSSStyleSheet();
      s.replaceSync(css);
      document.adoptedStyleSheets = document.adoptedStyleSheets.concat([s]);
    } catch (e) {
      var st = document.createElement('style');
      st.textContent = css;
      (document.head || document.documentElement).appendChild(st);
    }
  }

  // ---------- общая косметика: классы/id, которые встречаются на странице
  var seenC = new Set(), seenI = new Set(), pend = new Set(), timer = 0, genOn = false;
  function scan(el, nc, ni) {
    if (el.id && !seenI.has(el.id)) { seenI.add(el.id); ni.push(el.id); }
    var cl = el.classList;
    if (cl) for (var i = 0; i < cl.length; i++) if (!seenC.has(cl[i])) { seenC.add(cl[i]); nc.push(cl[i]); }
  }
  var attrPend = new Set();
  function flush() {
    timer = 0;
    var nc = [], ni = [];
    // новые узлы — со всем поддеревом, сменившийся класс — только сам элемент
    pend.forEach(function (root) {
      if (!root.isConnected) return;
      scan(root, nc, ni);
      var all = root.querySelectorAll('[class],[id]');
      for (var i = 0; i < all.length; i++) scan(all[i], nc, ni);
    });
    attrPend.forEach(function (el) { scan(el, nc, ni); });
    pend.clear();
    attrPend.clear();
    if (nc.length || ni.length) post({ t: 'gen', c: nc, i: ni });
  }
  function queue(n) { pend.add(n); if (!timer) timer = setTimeout(flush, 400); }
  function queueAttr(n) { attrPend.add(n); if (!timer) timer = setTimeout(flush, 400); }
  window.__monoGeneric = function () {
    if (genOn || (window.__monoCos && window.__monoCos.gh)) return;
    genOn = true;
    queue(document.documentElement);
    new MutationObserver(function (ms) {
      for (var i = 0; i < ms.length; i++) {
        var m = ms[i];
        if (m.type === 'attributes') queueAttr(m.target);
        else for (var j = 0; j < m.addedNodes.length; j++) if (m.addedNodes[j].nodeType === 1) queue(m.addedNodes[j]);
      }
    }).observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ['class', 'id'] });
  };

  // одна таблица для общих правил: правила дописываются, а не плодятся новые таблицы
  var genSheet = null;
  function addGeneric(css) {
    if (!css) return;
    try {
      if (!genSheet) {
        genSheet = new CSSStyleSheet();
        document.adoptedStyleSheets = document.adoptedStyleSheets.concat([genSheet]);
      }
      genSheet.insertRule(css, genSheet.cssRules.length);
    } catch (e) {}
  }
  W.addEventListener('message', function (e) {
    var m = e.data || {};
    if (m.t === 'gen') addGeneric(m.css);
    else if (m.t === 'pw-a') gotCreds(m.list);
  });

  // ---------- предзагрузка: ссылки своего сайта грузятся при наведении
  function speculate() {
    if (!HTMLScriptElement.supports || !HTMLScriptElement.supports('speculationrules')) return;
    var s = document.createElement('script');
    s.type = 'speculationrules';
    s.textContent = JSON.stringify({
      prefetch: [{
        source: 'document',
        where: { and: [
          { href_matches: '/*' },
          { not: { href_matches: '/*(logout|logoff|signout|sign-out|exit|delete|remove|unsubscribe)*' } },
          { not: { selector_matches: '[rel~=nofollow], [download], [target=_blank]' } }
        ] },
        eagerness: 'moderate'
      }]
    });
    (document.head || document.documentElement).appendChild(s);
  }

  function ready() {
    if (window.__monoCos === undefined) post({ t: 'cos' });
    else window.__monoGeneric();
    try { speculate(); } catch (e) {}
    scanPw();
    new MutationObserver(function () {
      if (!pwTimer) pwTimer = setTimeout(function () { pwTimer = 0; scanPw(); }, 500);
    }).observe(document.documentElement, { childList: true, subtree: true });
  }

  // ---------- пароли
  var asked = false, creds = null, chosen = null, lastUser = '', pwTimer = 0;
  function vis(el) { return !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length); }
  function pwFields() {
    return [].slice.call(document.querySelectorAll('input[type=password]')).filter(function (el) { return vis(el) && !el.disabled && !el.readOnly; });
  }
  function isUserField(i) {
    if (!/^(text|email|tel|)$/.test(i.type) || i.disabled || !vis(i)) return false;
    var a = ((i.autocomplete || '') + ' ' + (i.name || '') + ' ' + (i.id || '')).toLowerCase();
    return i.type === 'email' || /user|login|email|mail|phone|account|ident/.test(a);
  }
  function userFor(pw) {
    var scope = pw.form || pw.closest('form') || document;
    var ins = [].slice.call(scope.querySelectorAll('input')), u = null;
    for (var k = 0; k < ins.length; k++) {
      var i = ins[k];
      if (i === pw) break;
      if (/^(text|email|tel|)$/.test(i.type) && vis(i) && !i.disabled) u = i;
    }
    return u;
  }
  function setVal(el, v) {
    var d = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');
    el.focus();
    d.set.call(el, v);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }
  function userFields() {
    return [].slice.call(document.querySelectorAll('input')).filter(isUserField);
  }
  function scanPw() {
    var f = pwFields();
    if (!asked && (f.length || userFields().length)) { asked = true; post({ t: 'pw-q' }); }
    if (creds) offer();
  }
  function gotCreds(list) { creds = list; offer(); }
  function fill(c, pw) {
    chosen = c;
    var p = pw || pwFields()[0];
    var u = p ? userFor(p) : userFields()[0];
    if (u && c.u) setVal(u, c.u);
    if (p) setVal(p, c.p);
    hidePick();
  }
  function offer() {
    if (!creds || !creds.length) return;
    var fields = pwFields().concat(userFields());
    fields.forEach(function (el) {
      if (el.__monoPw) return;
      el.__monoPw = 1;
      el.addEventListener('focus', function () { showPick(el); });
    });
    // одна учётка и пустые поля — заполняем сразу
    var p = pwFields()[0];
    if (p && !p.value) {
      if (chosen) fill(chosen, p);
      else if (creds.length === 1) fill(creds[0], p);
    }
  }
  var pick = null;
  function hidePick() { if (pick) { pick.remove(); pick = null; } }
  function showPick(el) {
    if (!creds || !creds.length || (el.value && el.type === 'password')) return;
    hidePick();
    var r = el.getBoundingClientRect();
    pick = document.createElement('div');
    pick.style.cssText = 'all:initial;position:fixed;z-index:2147483647;left:' + r.left + 'px;top:' + (r.bottom + 4) + 'px;min-width:' + Math.max(220, r.width) + 'px';
    var root = pick.attachShadow({ mode: 'closed' });
    var css = '<style>div{font:13px "Segoe UI",system-ui,sans-serif;background:#141414;color:#ededed;border:1px solid #2c2c2c;border-radius:10px;padding:4px;box-shadow:0 8px 28px rgba(0,0,0,.45)}' +
      'b{display:block;padding:8px 10px;border-radius:7px;font-weight:500;cursor:pointer}b:hover{background:#262626}i{display:block;font-style:normal;color:#7c7c7c;font-size:11px;padding:4px 10px 2px}</style>';
    var html = css + '<div><i>Mono · сохранённые пароли</i>';
    creds.forEach(function (c, k) {
      html += '<b data-k="' + k + '">' + (c.u ? c.u.replace(/[<&]/g, '') : '(без логина)') + '</b>';
    });
    root.innerHTML = html + '</div>';
    root.addEventListener('mousedown', function (e) {
      e.preventDefault();
      var b = e.target.closest('b');
      if (b) fill(creds[+b.dataset.k], el.type === 'password' ? el : null);
    });
    document.documentElement.appendChild(pick);
    el.addEventListener('blur', function () { setTimeout(hidePick, 150); }, { once: true });
  }
  // запоминаем введённое при отправке формы
  function capture(scope) {
    var p = [].slice.call((scope || document).querySelectorAll('input[type=password]')).filter(function (i) { return i.value; })[0];
    if (!p) return;
    var u = userFor(p);
    post({ t: 'pw-save', u: (u && u.value) || lastUser, p: p.value });
  }
  document.addEventListener('input', function (e) {
    var t = e.target;
    if (t && t.tagName === 'INPUT' && isUserField(t)) lastUser = t.value;
  }, true);
  document.addEventListener('submit', function (e) { capture(e.target); }, true);
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && e.target && e.target.type === 'password') capture(e.target.form || document);
  }, true);
  document.addEventListener('click', function (e) {
    var b = e.target && e.target.closest && e.target.closest('button, input[type=submit], [role=button]');
    if (b) capture(b.form || b.closest('form') || document);
  }, true);

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', ready, { once: true });
  else ready();
})();
