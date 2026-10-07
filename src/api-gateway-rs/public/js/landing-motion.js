(function () {
  // Everything here is progressive: without JS, or with reduced motion,
  // the page is fully static and fully visible.
  if (!window.matchMedia('(prefers-reduced-motion: no-preference)').matches) return;
  document.documentElement.classList.add('js-motion');

  // Sections settle in once as they reach the viewport; cards in a grid stagger.
  var targets = document.querySelectorAll('section.block .s-head, section.block .card, .proof, .seq, .flow, .compare, #open-source .terminal, .split > div, .cta-band, .int-list, .faq-list');
  var io = new IntersectionObserver(function (entries) {
    entries.forEach(function (e) {
      if (!e.isIntersecting) return;
      e.target.classList.add('in');
      io.unobserve(e.target);
    });
  }, { threshold: 0.12, rootMargin: '0px 0px -40px 0px' });
  targets.forEach(function (el) {
    var sibs = el.parentElement ? Array.prototype.indexOf.call(el.parentElement.children, el) : 0;
    if (el.classList.contains('card')) el.style.transitionDelay = Math.min(sibs, 5) * 70 + 'ms';
    el.classList.add('reveal');
    io.observe(el);
  });

  // Proof numbers count up once.
  var nums = document.querySelectorAll('.proof-cell .n');
  var numIO = new IntersectionObserver(function (entries) {
    entries.forEach(function (e) {
      if (!e.isIntersecting) return;
      numIO.unobserve(e.target);
      var el = e.target, small = el.querySelector('small'), target = parseFloat(el.firstChild.nodeValue);
      if (!isFinite(target)) return;
      var dec = (String(target).split('.')[1] || '').length, t0 = performance.now();
      (function tick(now) {
        var p = Math.min((now - t0) / 900, 1), v = target * (1 - Math.pow(1 - p, 3));
        el.firstChild.nodeValue = v.toFixed(dec);
        if (p < 1) requestAnimationFrame(tick);
      })(t0);
    });
  }, { threshold: 0.5 });
  nums.forEach(function (n) { numIO.observe(n); });

  // Hero terminal replays the real run: commands type, output lines arrive.
  var pre = document.querySelector('.hero .terminal pre');
  if (pre) {
    var lines = pre.innerHTML.split('\n');
    pre.style.minHeight = pre.offsetHeight + 'px';  // reserve the final size so the hero doesn't shift
    pre.innerHTML = '';
    var cursor = document.createElement('span');
    cursor.className = 'cursor';
    pre.appendChild(cursor);
    var i = 0;
    function addLine(html) {
      var span = document.createElement('span');
      span.innerHTML = html + '\n';
      pre.insertBefore(span, cursor);
      return span;
    }
    function next() {
      if (i >= lines.length) return;
      var html = lines[i++];
      var m = html.match(/^(<span class="c-key">\$<\/span> )(.*)$/);
      if (!m) { addLine(html); setTimeout(next, html.trim() ? 170 : 90); return; }
      var span = addLine(m[1]), text = m[2], k = 0;
      span.innerHTML = m[1];
      (function type() {
        if (k <= text.length) { span.innerHTML = m[1] + text.slice(0, k++) + (k > text.length ? '\n' : ''); setTimeout(type, 28 + Math.random() * 40); }
        else setTimeout(next, 420);
      })();
    }
    setTimeout(next, 500);
  }
})();
