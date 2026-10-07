(function() {
  // Agent-network background, a live-updating visualization of what the
  // product actually does: a fleet of agents (nodes) sending actions
  // (packets) through a single reliability core. Now a fixed full-page
  // layer (see CSS: position:fixed, z-index:-1) rather than boxed into
  // one hero column, per request, for long connecting lines
  // instead of a small confined cluster, and so it shows behind every
  // section without its own opaque background, not just the hero.
  // Deliberately still simple: no CSS filters/blend-modes (that's what
  // froze the renderer in an earlier attempt at a grain-texture overlay
  //, see git history), capped devicePixelRatio, and the rAF loop stops
  // entirely when the tab itself isn't visible (Page Visibility API ,
  // there's no single "offscreen" section to watch anymore).
  var canvas = document.getElementById('agent-network');
  if (!canvas || !canvas.getContext) return;
  var ctx = canvas.getContext('2d');
  var motionOk = window.matchMedia('(prefers-reduced-motion: no-preference)').matches;
  var NODE_COUNT = 16;
  var W = 0, H = 0, dpr = 1;
  var nodes = [];
  var core = { x: 0, y: 0 };
  var packets = [];
  var lastSpawn = 0;

  function resize() {
    var host = canvas.parentElement.getBoundingClientRect(); W = host.width; H = host.height;
    dpr = Math.min(window.devicePixelRatio || 1, 2);
    canvas.width = W * dpr; canvas.height = H * dpr;
    canvas.style.width = W + 'px'; canvas.style.height = H + 'px';
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    core.x = W * 0.74; core.y = H * 0.18;
  }

  function initNodes() {
    nodes = [];
    for (var i = 0; i < NODE_COUNT; i++) {
      nodes.push({
        x: Math.random() * W, y: Math.random() * H,
        vx: (Math.random() - 0.5) * 0.4, vy: (Math.random() - 0.5) * 0.4,
        r: 2 + Math.random() * 2,
      });
    }
  }

  // Each packet is one action: agent A -> AgentRaaS (the core) -> destination B.
  // Endpoints are picked among the nodes nearest the core so routes stay short and visible.
  function spawnPacket() {
    var near = nodes.slice().sort(function(a, b) {
      return Math.hypot(a.x - core.x, a.y - core.y) - Math.hypot(b.x - core.x, b.y - core.y);
    }).slice(0, 8);
    var from = near[Math.floor(Math.random() * near.length)], to = from;
    while (to === from) to = near[Math.floor(Math.random() * near.length)];
    packets.push({ from: from, to: to, p: 0 });
  }

  function draw(now, animate) {
    ctx.clearRect(0, 0, W, H);

    nodes.forEach(function(n) {
      if (animate) {
        n.x += n.vx; n.y += n.vy;
        if (n.x < 0 || n.x > W) n.vx *= -1;
        if (n.y < 0 || n.y > H) n.vy *= -1;
      }
    });

    // Constellation lines, nearby nodes connect to each other, and every
    // node has a faint thread back to the core, so this reads as an
    // actual network graph rather than a scatter of unrelated dots.
    for (var i = 0; i < nodes.length; i++) {
      for (var j = i + 1; j < nodes.length; j++) {
        var dx = nodes[i].x - nodes[j].x, dy = nodes[i].y - nodes[j].y;
        var dist = Math.sqrt(dx * dx + dy * dy);
        if (dist < 170) {
          ctx.beginPath();
          ctx.strokeStyle = 'rgba(74,82,96,' + (0.14 * (1 - dist / 170)) + ')';
          ctx.lineWidth = 1;
          ctx.moveTo(nodes[i].x, nodes[i].y);
          ctx.lineTo(nodes[j].x, nodes[j].y);
          ctx.stroke();
        }
      }
      var cdx = nodes[i].x - core.x, cdy = nodes[i].y - core.y;
      var cdist = Math.sqrt(cdx * cdx + cdy * cdy);
      if (cdist < 320) {
        ctx.beginPath();
        ctx.strokeStyle = 'rgba(178,112,10,' + (0.14 * (1 - cdist / 320)) + ')';
        ctx.lineWidth = 1;
        ctx.moveTo(nodes[i].x, nodes[i].y);
        ctx.lineTo(core.x, core.y);
        ctx.stroke();
      }
    }

    nodes.forEach(function(n) {
      ctx.beginPath();
      ctx.fillStyle = 'rgba(74,82,96,0.45)';
      ctx.arc(n.x, n.y, n.r, 0, Math.PI * 2);
      ctx.fill();
    });

    // Wide soft halo, then the tighter core glow, then a thin ring (echoes
    // the logomark's ring-and-node shape) and the solid center, more
    // presence than a single small glow, still just plain fill/stroke.
    var halo = ctx.createRadialGradient(core.x, core.y, 0, core.x, core.y, 70);
    halo.addColorStop(0, 'rgba(201,138,30,0.10)');
    halo.addColorStop(1, 'rgba(201,138,30,0)');
    ctx.fillStyle = halo;
    ctx.beginPath(); ctx.arc(core.x, core.y, 70, 0, Math.PI * 2); ctx.fill();

    var grad = ctx.createRadialGradient(core.x, core.y, 0, core.x, core.y, 24);
    grad.addColorStop(0, 'rgba(178,112,10,0.6)');
    grad.addColorStop(1, 'rgba(201,138,30,0)');
    ctx.fillStyle = grad;
    ctx.beginPath(); ctx.arc(core.x, core.y, 24, 0, Math.PI * 2); ctx.fill();

    ctx.beginPath();
    ctx.strokeStyle = 'rgba(178,112,10,0.35)'; ctx.lineWidth = 1;
    ctx.arc(core.x, core.y, 13, 0, Math.PI * 2); ctx.stroke();
    ctx.beginPath(); ctx.fillStyle = '#B8781A'; ctx.arc(core.x, core.y, 4.5, 0, Math.PI * 2); ctx.fill();

    if (!animate) return;
    if (now - lastSpawn > 1100) { spawnPacket(); lastSpawn = now; }
    packets = packets.filter(function(pk) { return pk.p < 2; });
    packets.forEach(function(pk) {
      pk.p += 0.018;
      // the route this action takes, drawn faintly while it travels
      ctx.beginPath();
      ctx.strokeStyle = 'rgba(178,112,10,0.22)'; ctx.lineWidth = 1.2;
      ctx.moveTo(pk.from.x, pk.from.y); ctx.lineTo(core.x, core.y); ctx.lineTo(pk.to.x, pk.to.y);
      ctx.stroke();
      var a = pk.p < 1 ? pk.from : core, b = pk.p < 1 ? core : pk.to, t = pk.p < 1 ? pk.p : pk.p - 1;
      var x = a.x + (b.x - a.x) * t, y = a.y + (b.y - a.y) * t;
      ctx.beginPath(); ctx.fillStyle = '#B8781A'; ctx.arc(x, y, 2.8, 0, Math.PI * 2); ctx.fill();
      // destination node lights up as the action arrives
      if (pk.p > 1.85) { ctx.beginPath(); ctx.strokeStyle = 'rgba(178,112,10,0.5)'; ctx.arc(pk.to.x, pk.to.y, 7, 0, Math.PI * 2); ctx.stroke(); }
    });
  }

  resize();
  initNodes();
  window.addEventListener('resize', function() { resize(); initNodes(); }, { passive: true });

  if (!motionOk) {
    draw(0, false);
    return;
  }

  var rafId = null;
  function loop(now) { draw(now, true); rafId = requestAnimationFrame(loop); }
  function start() { if (!rafId) rafId = requestAnimationFrame(loop); }
  function stop() { if (rafId) { cancelAnimationFrame(rafId); rafId = null; } }

  document.addEventListener('visibilitychange', function() {
    if (document.visibilityState === 'visible') start(); else stop();
  });
  start();
})();
