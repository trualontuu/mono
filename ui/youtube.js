// YouTube: вырезаем рекламные блоки из ответов плеера до того, как их увидит страница,
// а если реклама всё-таки началась — мгновенно проматываем её.
(function () {
  var KEYS = ['adPlacements', 'adSlots', 'playerAds', 'adBreakHeartbeatParams'];
  function clean(o) {
    try {
      if (o && typeof o === 'object') {
        for (var i = 0; i < KEYS.length; i++) if (KEYS[i] in o) delete o[KEYS[i]];
        if (o.playerResponse) clean(o.playerResponse);
        if (o.response && o.response.playerResponse) clean(o.response.playerResponse);
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
        var v = document.querySelector('video');
        if (v && v.paused && !v.ended) v.play().catch(function () {});
      }
    }
  }
  setInterval(function () {
    killDialogs();
    var p = document.querySelector('.html5-video-player.ad-showing');
    if (p) {
      var vid = p.querySelector('video');
      if (vid && isFinite(vid.duration) && vid.duration > 0) { vid.muted = true; vid.currentTime = vid.duration; }
    }
    var b = document.querySelector('.ytp-skip-ad-button, .ytp-ad-skip-button, .ytp-ad-skip-button-modern');
    if (b) b.click();
  }, 300);
})();
