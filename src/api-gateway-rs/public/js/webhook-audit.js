(function () {
  function escapeHtml(str) {
    if (str === null || str === undefined) return '';
    return String(str).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  }

  const form = document.getElementById('audit-form');
  const urlInput = document.getElementById('audit-url');
  const submitBtn = document.getElementById('audit-submit');
  const errorBox = document.getElementById('audit-error');
  const results = document.getElementById('results');
  const verdictBanner = document.getElementById('verdict-banner');
  const verdictHeadline = document.getElementById('verdict-headline');
  const verdictDetail = document.getElementById('verdict-detail');
  const reqList = document.getElementById('req-list');

  const VERDICT_HEADLINES = {
    vulnerable: 'VULNERABLE: this endpoint looks like it processes duplicate requests as separate actions',
    inconclusive: 'INCONCLUSIVE: could not determine duplicate-processing risk from this run',
    likely_safe: 'LIKELY SAFE: responses were consistent with idempotent handling',
  };

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    errorBox.style.display = 'none';
    results.style.display = 'none';
    const url = urlInput.value.trim();
    if (!url) return;

    submitBtn.disabled = true;
    const originalText = submitBtn.innerHTML;
    submitBtn.innerHTML = '<span class="spinner"></span> Running…';

    try {
      const res = await fetch('/api/v1/tools/webhook-audit', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url }),
      });
      const data = await res.json();
      if (!res.ok) {
        errorBox.textContent = data.error || 'Could not run the audit.';
        errorBox.style.display = 'block';
        return;
      }

      verdictBanner.className = 'verdict-banner ' + data.verdict;
      verdictHeadline.textContent = VERDICT_HEADLINES[data.verdict] || data.verdict_label;
      verdictDetail.textContent = data.verdict_label;

      reqList.innerHTML = '';
      data.results.forEach((r) => {
        const row = document.createElement('div');
        row.className = 'req-row';
        const statusLine = r.status === null
          ? `<span class="err">Failed, ${escapeHtml(r.error)}</span>`
          : `<span class="${r.status < 400 ? 'ok' : 'err'}">HTTP ${r.status}</span> · ${r.latency_ms}ms`;
        row.innerHTML = `<div class="num">Request ${r.attempt}</div><div class="body"><div class="status-line">${statusLine}</div>${r.body_snippet ? `<div class="snippet">${escapeHtml(r.body_snippet)}</div>` : ''}</div>`;
        reqList.appendChild(row);
      });

      results.style.display = 'block';
      results.scrollIntoView({ behavior: 'smooth', block: 'start' });
    } catch (err) {
      errorBox.textContent = 'Could not reach the server. Try again in a moment.';
      errorBox.style.display = 'block';
    } finally {
      submitBtn.disabled = false;
      submitBtn.innerHTML = originalText;
    }
  });
})();
