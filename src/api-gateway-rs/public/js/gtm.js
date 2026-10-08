// Google Tag Manager, loaded only after the visitor accepts analytics
// cookies. The choice is kept in localStorage; until then a small banner
// asks once. Cloudflare Web Analytics is cookieless and not gated here.
(function () {
  var KEY = 'agentraas-analytics-consent';
  function choice() { try { return localStorage.getItem(KEY); } catch (e) { return null; } }
  function remember(v) { try { localStorage.setItem(KEY, v); } catch (e) {} }

  function loadGtm() {
    (function(w,d,s,l,i){w[l]=w[l]||[];w[l].push({'gtm.start':
    new Date().getTime(),event:'gtm.js'});var f=d.getElementsByTagName(s)[0],
    j=d.createElement(s),dl=l!='dataLayer'?'&l='+l:'';j.async=true;j.src=
    'https://www.googletagmanager.com/gtm.js?id='+i+dl;f.parentNode.insertBefore(j,f);
    })(window,document,'script','dataLayer','GTM-KMQ7H6W6');
  }

  if (choice() === 'yes') { loadGtm(); return; }
  if (choice() === 'no') return;

  function banner() {
    var b = document.createElement('div');
    b.setAttribute('role', 'region');
    b.setAttribute('aria-label', 'Cookie consent');
    b.style.cssText = 'position:fixed;left:16px;right:16px;bottom:16px;z-index:9999;max-width:560px;margin:0 auto;' +
      'background:#1f2329;color:#e8eaed;border:1px solid #3a3f47;border-radius:8px;padding:14px 16px;' +
      'font:14px/1.5 system-ui,sans-serif;box-shadow:0 6px 24px rgba(0,0,0,.3);display:flex;flex-wrap:wrap;gap:10px;align-items:center';
    var p = document.createElement('p');
    p.style.cssText = 'margin:0;flex:1 1 260px';
    p.innerHTML = 'Can we use Google Tag Manager to count visits and clicks? It sets cookies. <a href="/privacy" style="color:#f5b041">Privacy</a>';
    b.appendChild(p);
    function button(label, value, primary) {
      var btn = document.createElement('button');
      btn.type = 'button';
      btn.textContent = label;
      btn.style.cssText = 'cursor:pointer;border-radius:6px;padding:7px 14px;font:inherit;border:1px solid ' +
        (primary ? '#f5b041;background:#f5b041;color:#1f2329' : '#5a606a;background:transparent;color:#e8eaed');
      btn.addEventListener('click', function () {
        remember(value);
        b.remove();
        if (value === 'yes') loadGtm();
      });
      return btn;
    }
    b.appendChild(button('Decline', 'no', false));
    b.appendChild(button('Accept', 'yes', true));
    document.body.appendChild(b);
  }
  if (document.body) banner(); else document.addEventListener('DOMContentLoaded', banner);
})();
