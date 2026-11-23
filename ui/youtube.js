// YouTube: вырезаем рекламные блоки из ответов плеера до того, как их увидит страница,
// а если реклама всё-таки пошла (в т.ч. вшитая в поток) — проматываем её на x16 без звука.
(function () {
  if (window.top !== window) return;
  if (!/(^|\.)youtube\.com$/.test(location.hostname)) return;
  if (window.__monoYT) return;
  window.__monoYT = 1;
  var KEYS = ['adPlacements', 'adSlots', 'playerAds', 'adBreakHeartbeatParams'];
  function clean(o) {
    try {
      if (o && typeof o === 'object') {
        for (var i = 0; i < KEYS.length; i++) if (KEYS[i] in o) delete o[KEYS[i]];
        if (o.playerResponse) clean(o.playerResponse);
        if (o.response && o.response.playerResponse) clean(o.response.playerResponse);
        if (Array.isArray(o)) for (var k = 0; k < o.length && k < 8; k++) if (o[k] && o[k].playerResponse) clean(o[k].playerResponse);
      }
    } catch (e) {}
    return o;
  }
  var parse = JSON.parse;
  JSON.parse = function () { return clean(parse.apply(this, arguments)); };
  var rjson = Response.prototype.json;
  Response.prototype.json = function () { return rjson.call(this).then(clean); };
  var v;
  try {
    Object.defineProperty(window, 'ytInitialPlayerResponse', {
      configurable: true,
      get: function () { return v; },
      set: function (x) { v = clean(x); }
    });
  } catch (e) {}

  // окно «блокировщики запрещены» и пустые диалоги, у которых фильтры скрыли содержимое
  function killDialogs() {
    var ds = document.querySelectorAll('tp-yt-paper-dialog');
    for (var i = 0; i < ds.length; i++) {
      var d = ds[i];
      var cs = getComputedStyle(d);
      if (cs.display === 'none' || cs.visibility === 'hidden' || !d.getClientRects().length) continue;
      var enforce = d.querySelector('ytd-enforcement-message-view-model');
      var empty = !(d.innerText || '').trim();
      if (enforce || empty) {
        d.remove();
        var bd = document.querySelectorAll('tp-yt-iron-overlay-backdrop');
        for (var j = 0; j < bd.length; j++) bd[j].remove();
        document.body && document.body.style.removeProperty('overflow');
        var vv = document.querySelector('video');
        if (vv && vv.paused && !vv.ended) vv.play().catch(function () {});
      }
    }
  }

  var SKIP = '.ytp-skip-ad-button, .ytp-ad-skip-button, .ytp-ad-skip-button-modern, .ytp-ad-skip-button-container button, [id^="skip-button"] button, .ytp-skip-ad button';
  var inAd = false, saved = null;
  function ads() {
    var p = document.querySelector('.html5-video-player');
    if (!p) return;
    var vid = p.querySelector('video');
    var ad = p.classList.contains('ad-showing') || p.classList.contains('ad-interrupting');
    if (ad && vid) {
      if (!inAd) { inAd = true; saved = { m: vid.muted, r: vid.playbackRate === 16 ? 1 : vid.playbackRate }; }
      vid.muted = true;
      try { if (vid.playbackRate !== 16) vid.playbackRate = 16; } catch (e) {}
      // отдельный рекламный ролик — сразу в конец (вшитую рекламу так не трогаем: там длина = весь ролик)
      if (isFinite(vid.duration) && vid.duration > 0 && vid.duration <= 120 && vid.currentTime < vid.duration - 0.3) {
        try { vid.currentTime = vid.duration - 0.1; } catch (e) {}
      }
      if (vid.paused) vid.play().catch(function () {});
      var bs = document.querySelectorAll(SKIP);
      for (var i = 0; i < bs.length; i++) try { bs[i].click(); } catch (e) {}
    } else if (inAd) {
      inAd = false;
      if (vid && saved) {
        try { vid.playbackRate = saved.r; } catch (e) {}
        vid.muted = saved.m;
      }
      saved = null;
    }
  }
  function tick() { killDialogs(); ads(); }
  setInterval(tick, 250);
  // реагируем на смену класса плеера сразу, не дожидаясь таймера
  function watch() {
    var p = document.querySelector('.html5-video-player');
    if (!p) return setTimeout(watch, 500);
    new MutationObserver(ads).observe(p, { attributes: true, attributeFilter: ['class'] });
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', watch, { once: true });
  else watch();
})();
