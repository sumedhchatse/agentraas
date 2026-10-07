(function () {
  document.querySelectorAll('[data-copy]').forEach(function (b) {
    b.addEventListener('click', function () {
      navigator.clipboard.writeText(b.getAttribute('data-copy')).then(function () {
        var t = b.textContent; b.textContent = 'Copied'; setTimeout(function () { b.textContent = t; }, 1400);
      });
    });
  });
  // latest published version, straight from PyPI (it allows cross-origin reads)
  fetch('https://pypi.org/pypi/agentraas/json').then(function (r) { return r.ok ? r.json() : null; }).then(function (d) {
    var el = document.getElementById('pypi-version');
    if (d && d.info && el) el.textContent = 'v' + d.info.version;
  }).catch(function () {});
})();
