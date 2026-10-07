(function () {
  // Escapes user-controlled data before interpolating into innerHTML.
  // Needed because email validation intentionally allows a wide character
  // set (RFC-compliant email addresses can contain many special characters)
  // — the correct fix for that is escaping at render time, not
  // over-restricting a format that has legitimate reasons to be permissive.
  function escapeHtml(str) {
    if (str === null || str === undefined) return '';
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  // Swaps a submit button's text to show what's happening, instead of just
  // going inert with no explanation. Returns a function that restores the
  // original text — call it in a finally block so it always runs.
  function withLoadingText(btn, loadingText) {
    const original = btn.textContent;
    btn.textContent = loadingText;
    return () => { btn.textContent = original; };
  }

  const authScreen = document.getElementById('auth-screen');
  const dashboard = document.getElementById('dashboard');
  const userInfo = document.getElementById('user-info');
  const accountBtn = document.getElementById('account-btn');
  const authForm = document.getElementById('auth-form');
  const authError = document.getElementById('auth-error');
  const authTitle = document.getElementById('auth-title');
  const authSub = document.getElementById('auth-sub');
  const authSubmit = document.getElementById('auth-submit');
  const switchText = document.getElementById('switch-text');
  const switchLink = document.getElementById('switch-link');
  const sessionBanner = document.getElementById('session-banner');

  let mode = 'login';
  let currentRange = '24h';
  let volumeChart, statusChart, serviceChart;
  let hasEverAuthenticated = false;
  let currentUserEmail = '';
  let currentUserOrgId = '';
  let currentPlanTier = 'free';
  // Every account gets every feature (no plans or tiers; Cloud is priced by
  // usage), so the console is the same for everyone. Only the label says
  // which kind of account this is.
  const ACCOUNT_LABELS = { free: 'Free Cloud', payg: 'Pay as you go' };
  function applyConsoleLayout(plan) {
    document.getElementById('console-label').textContent = ACCOUNT_LABELS[plan] || 'Console';
    document.getElementById('hitl-rules-btn').style.display = '';
    const membersBtn = document.getElementById('enterprise-btn');
    membersBtn.style.display = '';
    membersBtn.textContent = 'SSO & members';
  }
  let refreshTimer = null;
  let anyModalOpen = false;
  let activityLimit = 50;
  let activityRows = [];
  const MUTED = '#4A5260';
  const GRID = '#E4E7EB';
  const CHART_FONT = { family: "'Archivo', sans-serif", size: 11 };

  function setMode(next) {
    mode = next; authError.style.display = 'none';
    if (mode === 'login') {
      authTitle.textContent = 'Log in'; authSub.textContent = 'Watch what your agents are doing, in real time.';
      authSubmit.textContent = 'Log in'; switchText.textContent = 'Need an account?'; switchLink.textContent = 'Register';
      document.getElementById('password').autocomplete = 'current-password';
    } else {
      authTitle.textContent = 'Create an account'; authSub.textContent = 'One account per organization is enough to get started.';
      authSubmit.textContent = 'Create account'; switchText.textContent = 'Already have an account?'; switchLink.textContent = 'Log in';
      document.getElementById('password').autocomplete = 'new-password';
    }
  }
  switchLink.addEventListener('click', () => setMode(mode === 'login' ? 'register' : 'login'));

  authForm.addEventListener('submit', async (e) => {
    e.preventDefault(); authError.style.display = 'none'; sessionBanner.style.display = 'none'; authSubmit.disabled = true;
    const restoreText = withLoadingText(authSubmit, mode === 'login' ? 'Logging in…' : 'Registering…');
    const email = document.getElementById('email').value.trim();
    const password = document.getElementById('password').value;
    const endpoint = mode === 'login' ? '/api/v1/auth/login' : '/api/v1/auth/register';
    try {
      const res = await fetch(endpoint, { method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify({ email, password }) });
      const data = await res.json();
      if (!res.ok) {
        if (data.code === 'EMAIL_NOT_VERIFIED') {
          authError.innerHTML = `${data.error} <a href="#" id="inline-resend-link">Resend verification email</a>`;
          authError.style.display = 'block';
          document.getElementById('inline-resend-link').addEventListener('click', async (ev) => {
            ev.preventDefault();
            await fetch('/api/v1/auth/resend-verification', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ email }) });
            authError.textContent = 'If that account needs verification, a new link has been sent.';
          });
          return;
        }
        authError.textContent = data.error || 'Something went wrong.'; authError.style.display = 'block'; return;
      }
      if (mode === 'register') {
        window.dataLayer = window.dataLayer || [];
        window.dataLayer.push({ event: 'sign_up' });
        authScreen.style.display = 'none';
        document.getElementById('verify-notice-screen').style.display = 'block';
        const devBox = document.getElementById('verify-notice-dev');
        if (data.dev_verify_url) {
          devBox.style.display = 'block';
          devBox.innerHTML = `No email server configured on this deployment — verify directly: <a href="${data.dev_verify_url}">${data.dev_verify_url}</a>`;
        } else {
          devBox.style.display = 'none';
        }
        return;
      }
      showDashboard(data.user);
    } catch (err) { authError.textContent = 'Could not reach the server.'; authError.style.display = 'block'; }
    finally { authSubmit.disabled = false; restoreText(); }
  });

  document.getElementById('verify-back-to-login-link').addEventListener('click', () => {
    document.getElementById('verify-notice-screen').style.display = 'none';
    authScreen.style.display = 'block';
  });

  function showLoggedOut() {
    stopAutoRefresh(); hasEverAuthenticated = false;
    dashboard.style.display = 'none'; userInfo.style.display = 'none'; authScreen.style.display = 'block';
  }
  document.getElementById('logout-btn').addEventListener('click', async () => {
    await fetch('/api/v1/auth/logout', { method: 'POST', credentials: 'include' });
    showLoggedOut();
  });
  document.getElementById('logout-all-btn').addEventListener('click', async () => {
    await fetch('/api/v1/auth/logout-all', { method: 'POST', credentials: 'include' });
    document.getElementById('account-modal-overlay').style.display = 'none'; anyModalOpen = false;
    showLoggedOut();
  });

  document.querySelectorAll('.ranges button').forEach((btn) => {
    btn.addEventListener('click', () => {
      document.querySelectorAll('.ranges button').forEach((b) => b.classList.remove('active'));
      btn.classList.add('active'); currentRange = btn.dataset.range; activityLimit = 50; loadAll();
      if (reliabilityModalOverlay.style.display === 'flex') loadReliabilityReport();
    });
  });
  document.getElementById('refresh-btn').addEventListener('click', async () => {
    const btn = document.getElementById('refresh-btn');
    if (btn.disabled) return; // already refreshing, ignore extra clicks
    const restoreText = withLoadingText(btn, 'Refreshing…');
    btn.disabled = true;
    try { await loadAll(); } finally { btn.disabled = false; restoreText(); }
  });
  document.getElementById('live-test-btn').addEventListener('click', async () => {
    const btn = document.getElementById('live-test-btn');
    if (btn.disabled) return;
    const banner = document.getElementById('live-test-result');
    const restoreText = withLoadingText(btn, 'Firing 8 identical requests…');
    btn.disabled = true;
    banner.style.display = 'block';
    banner.textContent = 'Sending 8 identical requests to a sandbox endpoint through the real dedup pipeline…';
    try {
      const res = await fetch('/api/v1/demo/live-test', { method: 'POST', credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) {
        banner.textContent = "Couldn't run the live test — try again in a moment.";
        return;
      }
      const r = await res.json();
      banner.innerHTML = `Sent <strong>${r.sent}</strong> identical requests to the sandbox endpoint — `
        + `<strong>${r.executed}</strong> actually executed, `
        + `<strong>${r.deduplicated}</strong> caught as duplicates`
        + (r.errored ? `, ${r.errored} errored` : '')
        + `. See the rows below in Recent activity — same req/idempotency key, real audit log.`;
      await loadAll();
    } catch {
      banner.textContent = "Couldn't run the live test — try again in a moment.";
    } finally {
      btn.disabled = false; restoreText();
    }
  });
  document.getElementById('seed-btn').addEventListener('click', async () => {
    const btn = document.getElementById('seed-btn');
    if (btn.disabled) return; // already seeding, ignore extra clicks
    const restoreText = withLoadingText(btn, 'Seeding…');
    btn.disabled = true;
    try {
      await fetch('/api/v1/demo/seed', { method: 'POST', credentials: 'include' });
      await loadAll();
    } finally {
      btn.disabled = false; restoreText();
    }
  });
  document.getElementById('reset-demo-btn').addEventListener('click', async () => {
    const btn = document.getElementById('reset-demo-btn');
    if (btn.disabled) return;
    if (!confirm('Clear this org\'s activity log, dead-letter queue, custom actions, and health-check settings? This cannot be undone.')) return;
    const restoreText = withLoadingText(btn, 'Resetting…');
    btn.disabled = true;
    try {
      await fetch('/api/v1/demo/reset', { method: 'POST', credentials: 'include' });
      await loadAll();
    } finally {
      btn.disabled = false; restoreText();
    }
  });
  document.getElementById('export-btn').addEventListener('click', () => { window.location.href = '/api/v1/export/csv'; });

  function showDashboard(user) {
    authScreen.style.display = 'none'; dashboard.style.display = 'block'; userInfo.style.display = 'flex';
    currentUserEmail = user.email;
    currentUserOrgId = user.org_id;
    accountBtn.textContent = user.email;
    currentPlanTier = user.plan || 'free';
    applyConsoleLayout(currentPlanTier);
    hasEverAuthenticated = true;
    sessionBanner.style.display = 'none';

    const isCloud = user.deployment_mode === 'cloud';
    document.getElementById('admin-btn').style.display = (user.is_admin && isCloud) ? 'flex' : 'none';

    // Seed/reset data is an admin- or designated-demo-account convenience
    // for testing/demoing — regular users have their own real activity to
    // look at, not a reason to fabricate demo data. Export CSV is
    // available to everyone; it exports only their own data.
    const canSeed = isCloud && (user.is_admin || user.is_demo);
    document.getElementById('seed-btn').style.display = canSeed ? 'flex' : 'none';
    document.getElementById('reset-demo-btn').style.display = canSeed ? 'flex' : 'none';
    document.getElementById('export-btn').style.display = 'flex';

    checkWelcomeBannerState();
    loadAll();
    startAutoRefresh();
  }

  async function checkWelcomeBannerState() {
    try {
      const res = await fetch('/api/v1/agents/keys', { credentials: 'include' });
      const keys = await res.json();
      document.getElementById('welcome-banner').style.display = (Array.isArray(keys) && keys.length === 0) ? 'flex' : 'none';
    } catch (err) { /* not critical — just skip showing the banner if this fails */ }
  }
  function loadAll() {
    return Promise.all([loadStats(), loadServices(), loadRecent(), loadAgents(), loadTimeseries(), loadByService(), loadUsage()]);
  }

  async function loadUsage() {
    try {
      const res = await fetch('/api/v1/usage', { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) return;
      const data = await res.json();
      const row = document.getElementById('usage-bar-row');
      const fill = document.getElementById('usage-bar-fill');
      const label = document.getElementById('usage-bar-label');
      const banner = document.getElementById('usage-over-limit-banner');
      row.style.display = 'flex';
      fill.classList.remove('near-limit', 'over-limit');
      banner.style.display = 'none';

      // Self-hosted deployments have no limit on any tier (see LICENSE.md) —
      // the bar just shows a full, uncapped track with the raw call count.
      if (data.unlimited || data.limit == null) {
        fill.style.width = '100%';
        label.textContent = `${data.total.toLocaleString()} calls this month (self-hosted — unlimited)`;
        return;
      }

      const pct = Math.min(100, (data.total / data.limit) * 100);
      fill.style.width = `${pct}%`;

      const isOverLimit = data.total >= data.limit;
      if (isOverLimit) fill.classList.add('over-limit');
      else if (pct >= 80) fill.classList.add('near-limit');

      if (isOverLimit) {
        banner.className = data.enforced ? 'cloud' : 'exempt';
        banner.style.display = 'flex';
        if (data.enforced) {
          banner.innerHTML = `<span>You've reached the free Cloud account's ${data.limit.toLocaleString()}/month limit, so new actions are paused until next month. Self-hosting is free with no limit.</span><a class="btn filled" href="/docs#self-hosting">Self-host free</a>`;
        } else {
          banner.innerHTML = `<span>You've passed the free Cloud account's ${data.limit.toLocaleString()}/month limit (this account is exempt). Self-hosting is free with no limit.</span><a class="btn filled" href="/docs#self-hosting">Self-host free</a>`;
        }
      }

      const modeLabel = data.enforced ? '' : ' (exempt from limit)';
      label.textContent = `${data.total.toLocaleString()} / ${data.limit.toLocaleString()} calls this month${modeLabel}`;
    } catch (err) { console.error('loadStats failed:', err); }
  }

  function startAutoRefresh() {
    stopAutoRefresh();
    refreshTimer = setInterval(() => {
      if (document.hidden || anyModalOpen) return; // don't refresh in background tabs or mid-form-entry
      loadAll();
    }, 12000);
    startUsageStream();
  }
  function stopAutoRefresh() { if (refreshTimer) { clearInterval(refreshTimer); refreshTimer = null; } stopUsageStream(); }

  // ─── Real-time usage count (SSE) ───
  // The call count updates the instant an action happens, instead of
  // waiting for the next 12s poll above — see /api/v1/usage/stream in
  // src/core/dashboard. The 12s poll above still runs as a fallback (also
  // covers every other stat panel, which aren't SSE-driven).
  let usageEventSource = null;
  let usageStreamDebounce = null;
  function startUsageStream() {
    if (usageEventSource || typeof EventSource === 'undefined') return;
    usageEventSource = new EventSource('/api/v1/usage/stream', { withCredentials: true });
    usageEventSource.onmessage = () => {
      // The SSE payload carries only the org that just changed, not the
      // full aggregated/limit/enforced shape loadUsage() needs — treat it
      // purely as a "something changed, refresh now" signal. Debounced so
      // a rapid burst of actions doesn't fire a request per event.
      clearTimeout(usageStreamDebounce);
      usageStreamDebounce = setTimeout(loadUsage, 250);
    };
    usageEventSource.onerror = () => {
      // EventSource auto-reconnects on its own; the 12s poll covers any gap
      // in the meantime, so there's nothing extra to do here.
    };
  }
  function stopUsageStream() {
    clearTimeout(usageStreamDebounce);
    if (usageEventSource) { usageEventSource.close(); usageEventSource = null; }
  }

  function formatDuration(ms) {
    if (ms === null || ms === undefined) return '—';
    const n = Number(ms);
    return n < 1000 ? `${Math.round(n)}ms` : `${(n / 1000).toFixed(2)}s`;
  }
  function timeOnly(iso) { return new Date(iso).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }); }

  async function loadStats() {
    try {
      const res = await fetch(`/api/v1/dashboard/stats?range=${currentRange}`, { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      const s = await res.json();
      document.getElementById('stat-total').textContent = s.total ?? '0';
      document.getElementById('stat-success').textContent = s.success ?? '0';
      document.getElementById('stat-deduplicated').textContent = s.deduplicated ?? '0';
      document.getElementById('stat-errors-blocked').textContent = (Number(s.blocked||0) + Number(s.errors||0));
      document.getElementById('stat-duration').textContent = formatDuration(s.avg_duration);

      const data = {
        labels: ['Success', 'Deduplicated', 'Blocked', 'Errors'],
        datasets: [{ data: [Number(s.success)||0, Number(s.deduplicated)||0, Number(s.blocked)||0, Number(s.errors)||0],
          backgroundColor: ['#4E8B6B', '#D9B54B', '#D9463F', '#6B3226'], borderColor: '#FFFFFF', borderWidth: 2 }],
      };
      const ctx = document.getElementById('status-chart');
      if (statusChart) { statusChart.data = data; statusChart.update(); }
      else statusChart = new Chart(ctx, { type: 'doughnut', data, options: { cutout: '68%', plugins: { legend: { position: 'bottom', labels: { color: MUTED, font: CHART_FONT, padding: 14, boxWidth: 10 } } } } });
    } catch (err) { console.error('loadTimeseries failed:', err); }
  }

  async function loadTimeseries() {
    try {
      const res = await fetch(`/api/v1/dashboard/timeseries?range=${currentRange}`, { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      const s = await res.json();
      const labels = s.points.map(p => { const d = new Date(p.bucket); return s.bucket === 'hour' ? d.toLocaleTimeString([], { hour: '2-digit' }) : d.toLocaleDateString([], { month: 'short', day: 'numeric' }); });
      const values = s.points.map(p => Number(p.total));
      const data = { labels, datasets: [{ data: values, backgroundColor: '#2B7FB8', borderRadius: 3, barPercentage: 0.5, categoryPercentage: 0.9 }] };
      const ctx = document.getElementById('volume-chart');
      if (volumeChart) { volumeChart.data = data; volumeChart.update(); }
      else volumeChart = new Chart(ctx, { type: 'bar', data, options: { plugins: { legend: { display: false } }, scales: { x: { ticks: { color: MUTED, font: CHART_FONT }, grid: { display: false } }, y: { ticks: { color: MUTED, font: CHART_FONT, precision: 0 }, grid: { color: GRID } } } } });
    } catch (err) { console.error('loadByService failed:', err); }
  }

  async function loadByService() {
    try {
      const res = await fetch(`/api/v1/dashboard/by-service?range=${currentRange}`, { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      const s = await res.json();
      const labels = s.services.map(r => r.service);
      const values = s.services.map(r => Number(r.total));
      const data = { labels, datasets: [{ data: values, backgroundColor: '#6E86A8', borderRadius: 3, maxBarThickness: 20 }] };
      const ctx = document.getElementById('service-chart');
      if (serviceChart) { serviceChart.data = data; serviceChart.update(); }
      else serviceChart = new Chart(ctx, { type: 'bar', data, options: { indexAxis: 'y', plugins: { legend: { display: false } }, scales: { x: { ticks: { color: MUTED, font: CHART_FONT, precision: 0 }, grid: { color: GRID } }, y: { ticks: { color: MUTED, font: CHART_FONT }, grid: { display: false } } } } });
    } catch (err) { console.error('loadServices failed:', err); }
  }

  async function loadServices() {
    try {
      const res = await fetch('/api/v1/services', { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      const services = await res.json();
      const strip = document.getElementById('service-strip');
      strip.innerHTML = '';
      for (const svc of services) {
        const healthy = svc.circuit_state !== 'open';
        const div = document.createElement('div');
        div.className = 'service-pill';
        // Idle = no calls today and nothing wrong; hidden on phones (see the 560px CSS).
        if (healthy && !Number(svc.actions_24h)) div.classList.add('idle');
        div.innerHTML = `<span class="heartbeat ${healthy ? '' : 'down'}"></span><span class="name">${escapeHtml(svc.name.charAt(0).toUpperCase() + svc.name.slice(1))}</span><span class="count">${escapeHtml(String(svc.actions_24h))} today</span>`;
        strip.appendChild(div);
      }
    } catch (err) {}
  }

  async function loadRecent() {
    const errorBox = document.getElementById('activity-error');
    try {
      const res = await fetch(`/api/v1/recent?limit=${activityLimit}`, { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) throw new Error('bad status');
      errorBox.style.display = 'none';
      activityRows = await res.json();
      renderActivity();
    } catch (err) {
      errorBox.style.display = 'flex';
    }
  }

  function renderActivity() {
    const search = document.getElementById('activity-search').value.trim().toLowerCase();
    const statusFilter = document.getElementById('activity-status-filter').value;
    const sortMode = document.getElementById('activity-sort').value;

    let rows = activityRows.filter((row) => {
      if (statusFilter && row.status !== statusFilter) return false;
      if (search) {
        const haystack = `${row.service} ${row.action} ${row.org_id} ${row.agent_id} ${row.run_id||''} ${row.step_id||''}`.toLowerCase();
        if (!haystack.includes(search)) return false;
      }
      return true;
    });

    rows = rows.slice().sort((a, b) => {
      if (sortMode === 'oldest') return new Date(a.created_at) - new Date(b.created_at);
      if (sortMode === 'slowest') return (Number(b.duration_ms)||0) - (Number(a.duration_ms)||0);
      if (sortMode === 'fastest') return (Number(a.duration_ms)||0) - (Number(b.duration_ms)||0);
      return new Date(b.created_at) - new Date(a.created_at); // newest (default)
    });

    const body = document.getElementById('activity-body');
    const empty = document.getElementById('empty-state');
    body.innerHTML = '';
    if (!rows.length) { empty.style.display = 'block'; return; }
    empty.style.display = 'none';
    for (const row of rows) {
      const div = document.createElement('div');
      div.className = `log-row ${row.status}`;
      // Runs/steps are optional (agent-supplied run_id/step_id tags on a
      // multi-step task) — most rows have neither, so the badge is only
      // shown when present rather than reserving space for it always.
      const runBadge = row.run_id
        ? ` <span class="run-badge" title="Click to filter this activity list to just this run" data-run-id="${escapeHtml(row.run_id)}" style="cursor:pointer;color:var(--muted);font-size:11px;">🔗 ${escapeHtml(row.step_id || row.run_id)}</span>`
          + ` <a href="#" class="see-run" title="Load the complete, unfiltered step list for this run" data-run-id="${escapeHtml(row.run_id)}" style="font-size:11px;">see full run</a>`
        : '';
      div.innerHTML = `
        <span class="time">${timeOnly(row.created_at)}</span>
        <span class="desc">${escapeHtml(row.service)}.${escapeHtml(row.action)} <span class="agent">— ${escapeHtml(row.org_id)}/${escapeHtml(row.agent_id)}</span>${runBadge}</span>
        <span class="dur">${formatDuration(row.duration_ms)}</span>
        <span class="status-word">${row.status}</span>`;
      const badgeEl = div.querySelector('.run-badge');
      if (badgeEl) {
        badgeEl.addEventListener('click', () => {
          document.getElementById('activity-search').value = badgeEl.dataset.runId;
          renderActivity();
        });
      }
      const seeRunEl = div.querySelector('.see-run');
      if (seeRunEl) {
        seeRunEl.addEventListener('click', (e) => { e.preventDefault(); openRunModal(seeRunEl.dataset.runId); });
      }
      body.appendChild(div);
    }
  }

  document.getElementById('activity-search').addEventListener('input', renderActivity);
  document.getElementById('activity-status-filter').addEventListener('change', renderActivity);
  document.getElementById('activity-sort').addEventListener('change', renderActivity);
  document.getElementById('load-more-btn').addEventListener('click', () => { activityLimit += 50; loadRecent(); });
  document.getElementById('activity-retry').addEventListener('click', loadRecent);

  async function loadAgents() {
    const errorBox = document.getElementById('agents-error');
    try {
      const res = await fetch('/api/v1/agents', { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) throw new Error('bad status');
      errorBox.style.display = 'none';
      const rows = await res.json();
      const list = document.getElementById('agents-list');
      list.innerHTML = '';
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;">No active agents in the last 24h.</div>'; return; }
      for (const row of rows) {
        const div = document.createElement('div');
        div.className = 'agent-item';
        div.innerHTML = `<div><div class="id">${escapeHtml(row.org_id)} / ${escapeHtml(row.agent_id)}</div><div class="meta">${row.total_actions} actions · last ${new Date(row.last_seen).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit'})}</div></div>`;
        list.appendChild(div);
      }
    } catch (err) {
      errorBox.style.display = 'flex';
    }
  }
  document.getElementById('agents-retry').addEventListener('click', loadAgents);

  function handleUnauthenticated() {
    stopAutoRefresh();
    dashboard.style.display = 'none'; userInfo.style.display = 'none'; authScreen.style.display = 'block';
    if (hasEverAuthenticated) { sessionBanner.style.display = 'block'; }
    hasEverAuthenticated = false;
  }

  // ─── Connect Agent modal ───
  const modalOverlay = document.getElementById('modal-overlay');
  const connectFormView = document.getElementById('connect-form-view');
  const connectResultView = document.getElementById('connect-result-view');
  const connectForm = document.getElementById('connect-form');
  const connectError = document.getElementById('connect-error');
  const connectSubmit = document.getElementById('connect-submit');

  async function loadConnectedAgentsList() {
    const countEl = document.getElementById('connected-agents-count');
    const listEl = document.getElementById('connected-agents-list');
    countEl.textContent = 'Loading…';
    listEl.innerHTML = '';
    try {
      const res = await fetch('/api/v1/agents/keys', { credentials: 'include' });
      const keys = (await res.json()).filter(k => !k.revoked_at);
      countEl.textContent = keys.length === 0 ? 'No agents connected yet.' : `${keys.length} agent${keys.length === 1 ? '' : 's'} connected`;
      for (const k of keys) {
        const row = document.createElement('div');
        row.className = 'cred-row';
        const lastUsed = k.last_used_at ? new Date(k.last_used_at).toLocaleDateString() : 'never used';
        row.innerHTML = `
          <div class="info">
            <div class="svc">${escapeHtml(k.org_id)} / ${escapeHtml(k.agent_id)} ${k.label ? `<span style="color:var(--muted);font-weight:400;">(${escapeHtml(k.label)})</span>` : ''}</div>
            <div class="meta">Key ${escapeHtml(k.key_prefix)}... · last used ${lastUsed}</div>
          </div>
          <button type="button" class="btn" data-regen-id="${k.id}" style="margin-right:6px;">Regenerate</button>
          <button class="revoke" data-revoke-id="${k.id}">Revoke</button>`;
        listEl.appendChild(row);
      }
    } catch (err) {
      countEl.textContent = 'Could not load connected agents.';
    }
  }

  document.getElementById('connect-agent-btn').addEventListener('click', () => {
    connectFormView.style.display = 'block'; connectResultView.style.display = 'none';
    connectError.style.display = 'none'; connectForm.reset(); modalOverlay.style.display = 'flex';
    anyModalOpen = true;
    document.getElementById('connect-org').focus();
    loadConnectedAgentsList();
  });

  document.getElementById('connected-agents-list').addEventListener('click', async (e) => {
    const revokeId = e.target.dataset.revokeId;
    const regenId = e.target.dataset.regenId;
    if (revokeId) {
      if (!confirm('Revoke this agent\'s key? It will stop working immediately.')) return;
      e.target.disabled = true;
      try {
        const res = await fetch(`/api/v1/agents/keys/${revokeId}`, { method: 'DELETE', credentials: 'include' });
        if (res.ok) loadConnectedAgentsList();
        else alert('Could not revoke key.');
      } catch (err) { alert('Could not reach the server.'); }
    } else if (regenId) {
      if (!confirm('Regenerate this key? The old one stops working immediately, and the new one is shown once.')) return;
      e.target.disabled = true;
      try {
        const res = await fetch(`/api/v1/agents/keys/${regenId}/regenerate`, { method: 'POST', credentials: 'include' });
        const data = await res.json();
        if (!res.ok) { alert(data.error || 'Could not regenerate key.'); return; }
        document.getElementById('result-key').textContent = data.api_key;
        document.getElementById('result-webhook').textContent = data.webhook_url;
        document.getElementById('result-mcp').textContent = data.mcp_url;
        document.getElementById('result-curl').textContent = `curl -X POST ${data.webhook_url} \\\n  -H "Authorization: Bearer ${data.api_key}" \\\n  -H "Content-Type: application/json" \\\n  -d '{"service":"mockpay","action":"charge.create","payload":{"amount":500,"currency":"usd","customer":"cus_123"}}'`;
        document.getElementById('result-n8n').textContent = `Method: POST\nURL: ${data.webhook_url}\nHeader: Authorization = Bearer ${data.api_key}\nBody (JSON): { "service": "mockpay", "action": "charge.create", "payload": { ... } }`;
        connectFormView.style.display = 'none'; connectResultView.style.display = 'block';
      } catch (err) { alert('Could not reach the server.'); }
    }
  });
  document.getElementById('modal-close').addEventListener('click', () => { modalOverlay.style.display = 'none'; anyModalOpen = false; });
  modalOverlay.addEventListener('click', (e) => { if (e.target === modalOverlay) { modalOverlay.style.display = 'none'; anyModalOpen = false; } });

  connectForm.addEventListener('submit', async (e) => {
    e.preventDefault(); connectError.style.display = 'none'; connectSubmit.disabled = true;
    const restoreText = withLoadingText(connectSubmit, 'Connecting…');
    const org_id = document.getElementById('connect-org').value.trim();
    const agent_id = document.getElementById('connect-agent').value.trim();
    const label = document.getElementById('connect-label').value.trim();
    try {
      const res = await fetch('/api/v1/agents/connect', { method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify({ org_id, agent_id, label: label || undefined }) });
      const data = await res.json();
      if (!res.ok) { connectError.textContent = data.error || 'Could not generate a connection.'; connectError.style.display = 'block'; return; }
      window.dataLayer = window.dataLayer || [];
      window.dataLayer.push({ event: 'agent_connected' });
      document.getElementById('result-key').textContent = data.api_key;
      document.getElementById('result-webhook').textContent = data.webhook_url;
      document.getElementById('result-mcp').textContent = data.mcp_url;
      document.getElementById('result-curl').textContent = `curl -X POST ${data.webhook_url} \\\n  -H "Authorization: Bearer ${data.api_key}" \\\n  -H "Content-Type: application/json" \\\n  -d '{"service":"mockpay","action":"charge.create","payload":{"amount":500,"currency":"usd","customer":"cus_123"}}'`;
      document.getElementById('result-n8n').textContent = `Method: POST\nURL: ${data.webhook_url}\nHeader: Authorization = Bearer ${data.api_key}\nBody (JSON): { "service": "mockpay", "action": "charge.create", "payload": { ... } }`;
      connectFormView.style.display = 'none'; connectResultView.style.display = 'block';
      document.getElementById('welcome-banner').style.display = 'none';
    } catch (err) { connectError.textContent = 'Could not reach the server.'; connectError.style.display = 'block'; }
    finally { connectSubmit.disabled = false; restoreText(); }
  });

  document.querySelectorAll('.copy').forEach((btn) => {
    btn.addEventListener('click', () => {
      const text = document.getElementById(btn.dataset.target).textContent;
      navigator.clipboard.writeText(text).then(() => { btn.textContent = 'Copied'; setTimeout(() => btn.textContent = 'Copy', 1200); });
    });
  });

  // ─── Credentials modal ───
  const credsModalOverlay = document.getElementById('creds-modal-overlay');
  const credsForm = document.getElementById('creds-form');
  const credsError = document.getElementById('creds-error');
  const credsSubmit = document.getElementById('creds-submit');
  const credsTypeSelect = document.getElementById('creds-type');
  const credsServiceSelect = document.getElementById('creds-service');
  let knownServices = [];

  function updateCredsFieldVisibility() {
    const isBasic = credsTypeSelect.value === 'basic';
    document.getElementById('creds-apikey-field').style.display = isBasic ? 'none' : 'block';
    document.getElementById('creds-username-field').style.display = isBasic ? 'block' : 'none';
    document.getElementById('creds-password-field').style.display = isBasic ? 'block' : 'none';
  }
  credsTypeSelect.addEventListener('change', updateCredsFieldVisibility);

  document.getElementById('credentials-btn').addEventListener('click', async () => {
    credsError.style.display = 'none'; credsForm.reset(); updateCredsFieldVisibility();
    if (!knownServices.length) {
      try { const res = await fetch('/api/v1/services', { credentials: 'include' }); if (res.ok) knownServices = await res.json(); } catch (err) {}
      credsServiceSelect.innerHTML = knownServices.map(s => `<option value="${s.name}">${s.name.charAt(0).toUpperCase() + s.name.slice(1)}</option>`).join('');
    }
    credsModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('creds-org').focus();
    loadCredentialsList();
  });
  document.getElementById('creds-modal-close').addEventListener('click', () => { credsModalOverlay.style.display = 'none'; anyModalOpen = false; });
  credsModalOverlay.addEventListener('click', (e) => { if (e.target === credsModalOverlay) { credsModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  credsForm.addEventListener('submit', async (e) => {
    e.preventDefault(); credsError.style.display = 'none'; credsSubmit.disabled = true;
    const restoreText = withLoadingText(credsSubmit, 'Saving…');
    const org_id = document.getElementById('creds-org').value.trim();
    const service = credsServiceSelect.value;
    const end_user_id = document.getElementById('creds-enduser').value.trim() || null;
    const isBasic = credsTypeSelect.value === 'basic';
    const credentials = isBasic
      ? { username: document.getElementById('creds-username').value.trim(), password: document.getElementById('creds-password').value.trim() }
      : { api_key: document.getElementById('creds-apikey').value.trim() };
    try {
      const res = await fetch('/api/v1/credentials', { method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify({ org_id, service, credentials, end_user_id }) });
      const data = await res.json();
      if (!res.ok) { credsError.textContent = data.error || 'Could not save credential.'; credsError.style.display = 'block'; return; }
      credsForm.reset(); updateCredsFieldVisibility(); loadCredentialsList();
    } catch (err) { credsError.textContent = 'Could not reach the server.'; credsError.style.display = 'block'; }
    finally { credsSubmit.disabled = false; restoreText(); }
  });

  async function loadCredentialsList() {
    const list = document.getElementById('creds-list');
    try {
      const res = await fetch('/api/v1/credentials', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No credentials saved yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        const svcLabel = escapeHtml(row.service.charAt(0).toUpperCase() + row.service.slice(1)) + (row.end_user_id ? ` — end-user ${escapeHtml(row.end_user_id)}` : '');
        div.innerHTML = `<div class="info"><div class="svc">${svcLabel} — ${escapeHtml(row.org_id)}</div><div class="meta">${escapeHtml(row.masked_preview)}</div></div><button class="revoke" data-id="${row.id}">Revoke</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.revoke').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/credentials/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadCredentialsList(); });
      });
    } catch (err) {}
  }

  // ─── Custom Actions modal ───
  const customModalOverlay = document.getElementById('custom-modal-overlay');
  const customForm = document.getElementById('custom-form');
  const customError = document.getElementById('custom-error');
  const customSubmit = document.getElementById('custom-submit');
  const customAuthType = document.getElementById('custom-authtype');

  function updateCustomFieldVisibility() {
    const t = customAuthType.value;
    document.getElementById('custom-headername-field').style.display = t === 'header' ? 'block' : 'none';
    document.getElementById('custom-apikey-field').style.display = (t === 'bearer' || t === 'header') ? 'block' : 'none';
    document.getElementById('custom-username-field').style.display = t === 'basic' ? 'block' : 'none';
    document.getElementById('custom-password-field').style.display = t === 'basic' ? 'block' : 'none';
  }
  customAuthType.addEventListener('change', updateCustomFieldVisibility);

  document.getElementById('custom-actions-btn').addEventListener('click', () => {
    customError.style.display = 'none'; customForm.reset(); updateCustomFieldVisibility();
    customModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('custom-org').focus();
    loadCustomActionsList();
  });
  document.getElementById('custom-modal-close').addEventListener('click', () => { customModalOverlay.style.display = 'none'; anyModalOpen = false; });
  customModalOverlay.addEventListener('click', (e) => { if (e.target === customModalOverlay) { customModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  customForm.addEventListener('submit', async (e) => {
    e.preventDefault(); customError.style.display = 'none'; customSubmit.disabled = true;
    const restoreText = withLoadingText(customSubmit, 'Registering…');
    const org_id = document.getElementById('custom-org').value.trim();
    const name = document.getElementById('custom-name').value.trim();
    const method = document.getElementById('custom-method').value;
    const target_url = document.getElementById('custom-url').value.trim();
    const auth_type = customAuthType.value;
    const auth_header_name = document.getElementById('custom-headername').value.trim();

    let credential;
    if (auth_type === 'basic') {
      credential = { username: document.getElementById('custom-username').value.trim(), password: document.getElementById('custom-password').value.trim() };
    } else if (auth_type === 'bearer' || auth_type === 'header') {
      credential = { api_key: document.getElementById('custom-apikey').value.trim() };
    }

    // "Header-Name: value" per line; a "secret:" value prefix marks it for
    // encryption-at-rest instead of being stored in the clear.
    const extra_headers = document.getElementById('custom-extra-headers').value.split('\n')
      .map((line) => line.trim()).filter(Boolean)
      .map((line) => {
        const idx = line.indexOf(':');
        if (idx === -1) return null;
        const name = line.slice(0, idx).trim();
        let value = line.slice(idx + 1).trim();
        const secret = value.startsWith('secret:');
        if (secret) value = value.slice('secret:'.length).trim();
        return name && value ? { name, value, secret } : null;
      }).filter(Boolean);

    const fanout_urls = document.getElementById('custom-fanout-urls').value.split(',')
      .map((u) => u.trim()).filter(Boolean);

    try {
      const res = await fetch('/api/v1/custom-actions', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, name, method, target_url, auth_type, auth_header_name: auth_header_name || undefined, credential, extra_headers, fanout_urls }),
      });
      const data = await res.json();
      if (!res.ok) { customError.textContent = data.error || 'Could not register action.'; customError.style.display = 'block'; return; }
      customForm.reset(); updateCustomFieldVisibility(); loadCustomActionsList();
    } catch (err) { customError.textContent = 'Could not reach the server.'; customError.style.display = 'block'; }
    finally { customSubmit.disabled = false; restoreText(); }
  });

  async function loadCustomActionsList() {
    const list = document.getElementById('custom-list');
    try {
      const res = await fetch('/api/v1/custom-actions', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No custom actions registered yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const extras = [];
        if (row.extra_header_count) extras.push(`${row.extra_header_count} extra header${row.extra_header_count === 1 ? '' : 's'}`);
        if (row.fanout_url_count) extras.push(`fans out to ${row.fanout_url_count}`);
        const extrasText = extras.length ? ` · ${extras.join(', ')}` : '';
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.name)} — ${escapeHtml(row.org_id)}</div><div class="meta">${row.method} ${escapeHtml(row.target_url)}${extrasText}</div></div><button class="revoke" data-id="${row.id}">Revoke</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.revoke').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/custom-actions/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadCustomActionsList(); });
      });
    } catch (err) {}
  }

  // ─── MCP Servers modal (MCP Custom Actions) ───
  const mcpsvrModalOverlay = document.getElementById('mcpsvr-modal-overlay');
  const mcpsvrForm = document.getElementById('mcpsvr-form');
  const mcpsvrError = document.getElementById('mcpsvr-error');
  const mcpsvrSubmit = document.getElementById('mcpsvr-submit');
  const mcpsvrAuthType = document.getElementById('mcpsvr-authtype');

  function updateMcpsvrFieldVisibility() {
    const t = mcpsvrAuthType.value;
    document.getElementById('mcpsvr-headername-field').style.display = t === 'header' ? 'block' : 'none';
    document.getElementById('mcpsvr-apikey-field').style.display = (t === 'bearer' || t === 'header') ? 'block' : 'none';
    document.getElementById('mcpsvr-username-field').style.display = t === 'basic' ? 'block' : 'none';
    document.getElementById('mcpsvr-password-field').style.display = t === 'basic' ? 'block' : 'none';
  }
  mcpsvrAuthType.addEventListener('change', updateMcpsvrFieldVisibility);

  document.getElementById('mcpsvr-btn').addEventListener('click', () => {
    mcpsvrError.style.display = 'none'; document.getElementById('mcpsvr-success').style.display = 'none'; mcpsvrForm.reset(); updateMcpsvrFieldVisibility();
    mcpsvrModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('mcpsvr-org').focus();
    loadMcpServersList();
  });
  document.getElementById('mcpsvr-modal-close').addEventListener('click', () => { mcpsvrModalOverlay.style.display = 'none'; anyModalOpen = false; });
  mcpsvrModalOverlay.addEventListener('click', (e) => { if (e.target === mcpsvrModalOverlay) { mcpsvrModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  mcpsvrForm.addEventListener('submit', async (e) => {
    e.preventDefault(); mcpsvrError.style.display = 'none'; document.getElementById('mcpsvr-success').style.display = 'none'; mcpsvrSubmit.disabled = true;
    const restoreText = withLoadingText(mcpsvrSubmit, 'Registering…');
    const org_id = document.getElementById('mcpsvr-org').value.trim();
    const name = document.getElementById('mcpsvr-name').value.trim();
    const target_url = document.getElementById('mcpsvr-url').value.trim();
    const auth_type = mcpsvrAuthType.value;
    const auth_header_name = document.getElementById('mcpsvr-headername').value.trim();

    let credential;
    if (auth_type === 'basic') {
      credential = { username: document.getElementById('mcpsvr-username').value.trim(), password: document.getElementById('mcpsvr-password').value.trim() };
    } else if (auth_type === 'bearer' || auth_type === 'header') {
      credential = { api_key: document.getElementById('mcpsvr-apikey').value.trim() };
    }

    try {
      const res = await fetch('/api/v1/mcp-servers', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, name, target_url, auth_type, auth_header_name: auth_header_name || undefined, credential }),
      });
      const data = await res.json();
      if (!res.ok) { mcpsvrError.textContent = data.error || 'Could not register server.'; mcpsvrError.style.display = 'block'; return; }
      const successEl = document.getElementById('mcpsvr-success');
      successEl.textContent = data.note || 'Registered.';
      successEl.style.display = 'block';
      mcpsvrForm.reset(); updateMcpsvrFieldVisibility(); loadMcpServersList();
    } catch (err) { mcpsvrError.textContent = 'Could not reach the server.'; mcpsvrError.style.display = 'block'; }
    finally { mcpsvrSubmit.disabled = false; restoreText(); }
  });

  async function loadMcpServersList() {
    const list = document.getElementById('mcpsvr-list');
    try {
      const res = await fetch('/api/v1/mcp-servers', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No MCP servers registered yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.name)} — ${escapeHtml(row.org_id)}</div><div class="meta">${escapeHtml(row.target_url)} · tools appear as ${escapeHtml(row.name)}.&lt;tool&gt;</div></div><button class="revoke" data-id="${row.id}">Revoke</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.revoke').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/mcp-servers/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadMcpServersList(); });
      });
    } catch (err) {}
  }

  // ─── Validation Rules modal ───
  const vrModalOverlay = document.getElementById('vr-modal-overlay');
  const vrForm = document.getElementById('vr-form');
  const vrError = document.getElementById('vr-error');
  const vrSubmit = document.getElementById('vr-submit');
  const vrFieldsContainer = document.getElementById('vr-fields-container');
  const drModalOverlay = document.getElementById('dr-modal-overlay');
  const drForm = document.getElementById('dr-form');
  const drError = document.getElementById('dr-error');
  const drSubmit = document.getElementById('dr-submit');
  let vrFieldRowId = 0;

  function createFieldRow(name, rules) {
    rules = rules || {};
    const id = 'vrf' + (vrFieldRowId++);
    const row = document.createElement('div');
    row.className = 'field-rule-row';
    row.dataset.rowId = id;
    row.innerHTML = `
      <div class="row1">
        <input type="text" class="vrf-name" placeholder="field name (e.g. amount)" value="${escapeHtml(name || '')}">
        <select class="vrf-type">
          <option value="">any type</option>
          <option value="string">string</option>
          <option value="number">number</option>
          <option value="boolean">boolean</option>
          <option value="array">array</option>
          <option value="object">object</option>
        </select>
        <label class="required-label"><input type="checkbox" class="vrf-required"> required</label>
        <button type="button" class="remove-field-btn">✕</button>
      </div>
      <div class="row2">
        <input type="text" class="vrf-min" placeholder="min (number)">
        <input type="text" class="vrf-max" placeholder="max (number)">
        <input type="text" class="vrf-minlength" placeholder="minLength">
        <input type="text" class="vrf-maxlength" placeholder="maxLength">
        <input type="text" class="vrf-enum" placeholder="enum: a,b,c">
        <select class="vrf-format">
          <option value="">no format check</option>
          <option value="email">email format</option>
          <option value="e164">phone (E.164)</option>
        </select>
      </div>
    `;
    row.querySelector('.vrf-type').value = rules.type || '';
    row.querySelector('.vrf-required').checked = !!rules.required;
    row.querySelector('.vrf-min').value = rules.min !== undefined ? rules.min : '';
    row.querySelector('.vrf-max').value = rules.max !== undefined ? rules.max : '';
    row.querySelector('.vrf-minlength').value = rules.minLength !== undefined ? rules.minLength : '';
    row.querySelector('.vrf-maxlength').value = rules.maxLength !== undefined ? rules.maxLength : '';
    row.querySelector('.vrf-enum').value = rules.enum ? rules.enum.join(',') : '';
    row.querySelector('.vrf-format').value = rules.format || '';
    row.querySelector('.remove-field-btn').addEventListener('click', () => row.remove());
    vrFieldsContainer.appendChild(row);
  }

  function collectFieldsFromRows() {
    const fields = {};
    vrFieldsContainer.querySelectorAll('.field-rule-row').forEach((row) => {
      const name = row.querySelector('.vrf-name').value.trim();
      if (!name) return;
      const rule = {};
      const type = row.querySelector('.vrf-type').value;
      if (type) rule.type = type;
      if (row.querySelector('.vrf-required').checked) rule.required = true;
      const min = row.querySelector('.vrf-min').value.trim();
      if (min !== '') rule.min = Number(min);
      const max = row.querySelector('.vrf-max').value.trim();
      if (max !== '') rule.max = Number(max);
      const minLength = row.querySelector('.vrf-minlength').value.trim();
      if (minLength !== '') rule.minLength = Number(minLength);
      const maxLength = row.querySelector('.vrf-maxlength').value.trim();
      if (maxLength !== '') rule.maxLength = Number(maxLength);
      const enumVal = row.querySelector('.vrf-enum').value.trim();
      if (enumVal) rule.enum = enumVal.split(',').map((s) => s.trim()).filter(Boolean);
      const format = row.querySelector('.vrf-format').value;
      if (format) rule.format = format;
      fields[name] = rule;
    });
    return fields;
  }

  function resetVrForm() {
    vrForm.reset();
    vrFieldsContainer.innerHTML = '';
    createFieldRow();
    // className, not .style.display — an inline style would permanently
    // outrank the #vr-test-result.pass/.fail CSS rules that show it again.
    document.getElementById('vr-test-result').className = '';
  }

  document.getElementById('vr-add-field-btn').addEventListener('click', () => createFieldRow());

  // ─── API Guard Templates ─── 1-click starting points for common APIs,
  // rather than asking everyone to write field rules from scratch. Each
  // only expresses what the validator can actually check (payload shape —
  // required/type/format/range) — e.g. "Calendly double-booking
  // prevention" from the pitch really needs a live lookup against
  // existing bookings, which this system doesn't do; that template just
  // guards the fields a booking call needs, labeled as such.
  const VR_TEMPLATES = {
    stripe_charge: {
      service: 'stripe', action: 'charge.create',
      fields: {
        amount: { type: 'number', required: true, min: 1 },
        currency: { type: 'string', required: true, minLength: 3, maxLength: 3 },
      },
    },
    whatsapp_message: {
      service: 'whatsapp', action: 'message.send',
      fields: {
        to: { type: 'string', required: true, format: 'e164' },
      },
    },
    hubspot_contact: {
      service: 'hubspot', action: 'contact.create',
      fields: {
        email: { type: 'string', required: true, format: 'email' },
      },
    },
    calendly_booking: {
      service: 'calendly', action: 'booking.create',
      fields: {
        start_time: { type: 'string', required: true },
        invitee_email: { type: 'string', required: true, format: 'email' },
      },
    },
  };

  document.getElementById('vr-template-select').addEventListener('change', (e) => {
    const template = VR_TEMPLATES[e.target.value];
    e.target.value = ''; // one-shot picker, not a persistent selection tied to the rule
    if (!template) return;
    document.getElementById('vr-service').value = template.service;
    document.getElementById('vr-action').value = template.action;
    vrFieldsContainer.innerHTML = '';
    for (const [fname, frules] of Object.entries(template.fields)) createFieldRow(fname, frules);
  });

  document.getElementById('validation-rules-btn').addEventListener('click', () => {
    vrError.style.display = 'none'; resetVrForm();
    vrModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('vr-org').focus();
    loadValidationRulesList();
  });
  document.getElementById('vr-modal-close').addEventListener('click', () => { vrModalOverlay.style.display = 'none'; anyModalOpen = false; });
  vrModalOverlay.addEventListener('click', (e) => { if (e.target === vrModalOverlay) { vrModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  document.getElementById('vr-test-btn').addEventListener('click', async () => {
    const resultEl = document.getElementById('vr-test-result');
    let payload;
    const raw = document.getElementById('vr-test-payload').value.trim();
    try { payload = raw ? JSON.parse(raw) : {}; } catch (err) {
      resultEl.className = 'fail'; resultEl.textContent = 'Sample payload is not valid JSON.'; return;
    }
    const fields = collectFieldsFromRows();
    try {
      const res = await fetch('/api/v1/validation-rules/test', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ fields, payload }),
      });
      const data = await res.json();
      if (!res.ok) { resultEl.className = 'fail'; resultEl.textContent = data.error || 'Could not test rule.'; return; }
      resultEl.className = data.valid ? 'pass' : 'fail';
      resultEl.textContent = data.valid ? '✓ Payload passes this rule.' : `✗ ${data.error}`;
    } catch (err) { resultEl.className = 'fail'; resultEl.textContent = 'Could not reach the server.'; }
  });

  vrForm.addEventListener('submit', async (e) => {
    e.preventDefault(); vrError.style.display = 'none'; vrSubmit.disabled = true;
    const restoreText = withLoadingText(vrSubmit, 'Saving…');
    const org_id = document.getElementById('vr-org').value.trim();
    const service = document.getElementById('vr-service').value.trim();
    const action = document.getElementById('vr-action').value.trim();
    const fields = collectFieldsFromRows();
    if (Object.keys(fields).length === 0) {
      vrError.textContent = 'Add at least one field.'; vrError.style.display = 'block';
      vrSubmit.disabled = false; restoreText(); return;
    }
    try {
      const res = await fetch('/api/v1/validation-rules', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, service, action, fields }),
      });
      const data = await res.json();
      if (!res.ok) { vrError.textContent = data.error || 'Could not save rule.'; vrError.style.display = 'block'; return; }
      resetVrForm(); loadValidationRulesList();
    } catch (err) { vrError.textContent = 'Could not reach the server.'; vrError.style.display = 'block'; }
    finally { vrSubmit.disabled = false; restoreText(); }
  });

  async function loadValidationRulesList() {
    const list = document.getElementById('vr-list');
    try {
      const res = await fetch('/api/v1/validation-rules', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No validation rules yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const fieldCount = Object.keys(row.fields || {}).length;
        const div = document.createElement('div');
        div.className = 'rule-summary-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.service)}.${escapeHtml(row.action)} — ${escapeHtml(row.org_id)}</div><div class="meta">${fieldCount} field rule${fieldCount === 1 ? '' : 's'}</div></div><div style="display:flex;gap:6px;"><button class="btn edit-rule" data-id="${row.id}" style="padding:6px 12px;font-size:12px;">Edit</button><button class="revoke delete-rule" data-id="${row.id}">Delete</button></div>`;
        list.appendChild(div);
        div.querySelector('.edit-rule').addEventListener('click', () => {
          document.getElementById('vr-org').value = row.org_id;
          document.getElementById('vr-service').value = row.service;
          document.getElementById('vr-action').value = row.action;
          vrFieldsContainer.innerHTML = '';
          for (const [fname, frules] of Object.entries(row.fields || {})) createFieldRow(fname, frules);
          if (!vrFieldsContainer.children.length) createFieldRow();
          document.getElementById('vr-org').scrollIntoView({ behavior: 'smooth', block: 'start' });
        });
      }
      list.querySelectorAll('.delete-rule').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/validation-rules/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadValidationRulesList(); });
      });
    } catch (err) {}
  }

  // ─── Dedup Rules ───
  function resetDrForm() { drForm.reset(); }

  document.getElementById('dedup-rules-btn').addEventListener('click', () => {
    drError.style.display = 'none'; resetDrForm();
    drModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('dr-org').focus();
    loadDedupRulesList();
  });
  document.getElementById('dr-modal-close').addEventListener('click', () => { drModalOverlay.style.display = 'none'; anyModalOpen = false; });
  drModalOverlay.addEventListener('click', (e) => { if (e.target === drModalOverlay) { drModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  drForm.addEventListener('submit', async (e) => {
    e.preventDefault(); drError.style.display = 'none'; drSubmit.disabled = true;
    const restoreText = withLoadingText(drSubmit, 'Saving…');
    const org_id = document.getElementById('dr-org').value.trim();
    const service = document.getElementById('dr-service').value.trim();
    const action = document.getElementById('dr-action').value.trim();
    const fields = document.getElementById('dr-fields').value.split(',').map((f) => f.trim()).filter(Boolean);
    if (fields.length === 0) {
      drError.textContent = 'Add at least one field.'; drError.style.display = 'block';
      drSubmit.disabled = false; restoreText(); return;
    }
    try {
      const res = await fetch('/api/v1/dedup-rules', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, service, action, fields }),
      });
      const data = await res.json();
      if (!res.ok) { drError.textContent = data.error || 'Could not save rule.'; drError.style.display = 'block'; return; }
      resetDrForm(); loadDedupRulesList();
    } catch (err) { drError.textContent = 'Could not reach the server.'; drError.style.display = 'block'; }
    finally { drSubmit.disabled = false; restoreText(); }
  });

  async function loadDedupRulesList() {
    const list = document.getElementById('dr-list');
    try {
      const res = await fetch('/api/v1/dedup-rules', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No dedup rules yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const fieldNames = (row.fields || []).join(', ');
        const div = document.createElement('div');
        div.className = 'rule-summary-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.service)}.${escapeHtml(row.action)} — ${escapeHtml(row.org_id)}</div><div class="meta">dedupes on: ${escapeHtml(fieldNames)}</div></div><div style="display:flex;gap:6px;"><button class="btn edit-rule" data-id="${row.id}" style="padding:6px 12px;font-size:12px;">Edit</button><button class="revoke delete-rule" data-id="${row.id}">Delete</button></div>`;
        list.appendChild(div);
        div.querySelector('.edit-rule').addEventListener('click', () => {
          document.getElementById('dr-org').value = row.org_id;
          document.getElementById('dr-service').value = row.service;
          document.getElementById('dr-action').value = row.action;
          document.getElementById('dr-fields').value = fieldNames;
          document.getElementById('dr-org').scrollIntoView({ behavior: 'smooth', block: 'start' });
        });
      }
      list.querySelectorAll('.delete-rule').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/dedup-rules/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadDedupRulesList(); });
      });
    } catch (err) {}
  }

  // ─── Output Pruning modal ───
  const pruningModalOverlay = document.getElementById('pruning-modal-overlay');
  const pruningError = document.getElementById('pruning-error');
  let pruningCurrentOrg = '';
  let pruningCurrentEnabled = false;

  document.getElementById('pruning-btn').addEventListener('click', () => {
    pruningError.style.display = 'none';
    document.getElementById('pruning-status').style.display = 'none';
    document.getElementById('pruning-org').value = '';
    pruningModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('pruning-org').focus();
  });
  document.getElementById('pruning-modal-close').addEventListener('click', () => { pruningModalOverlay.style.display = 'none'; anyModalOpen = false; });
  pruningModalOverlay.addEventListener('click', (e) => { if (e.target === pruningModalOverlay) { pruningModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  function renderPruningStatus() {
    document.getElementById('pruning-status').style.display = 'block';
    const badge = document.getElementById('pruning-status-badge');
    badge.textContent = pruningCurrentEnabled ? 'Enabled' : 'Disabled';
    badge.className = 'badge ' + (pruningCurrentEnabled ? 'verified' : 'unverified');
    document.getElementById('pruning-toggle-btn').textContent = pruningCurrentEnabled ? 'Disable' : 'Enable';
  }

  async function loadPruningStatus() {
    const orgId = document.getElementById('pruning-org').value.trim();
    if (!orgId) { pruningError.textContent = 'Enter an org ID first.'; pruningError.style.display = 'block'; return; }
    pruningError.style.display = 'none';
    try {
      const res = await fetch(`/api/v1/output-pruning?org_id=${encodeURIComponent(orgId)}`, { credentials: 'include' });
      const data = await res.json();
      if (!res.ok) { pruningError.textContent = data.error || 'Could not load status.'; pruningError.style.display = 'block'; return; }
      pruningCurrentOrg = orgId; pruningCurrentEnabled = !!data.enabled;
      renderPruningStatus();
    } catch (err) { pruningError.textContent = 'Could not reach the server.'; pruningError.style.display = 'block'; }
  }
  document.getElementById('pruning-load-btn').addEventListener('click', loadPruningStatus);
  document.getElementById('pruning-org').addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); loadPruningStatus(); } });

  document.getElementById('pruning-toggle-btn').addEventListener('click', async () => {
    const btn = document.getElementById('pruning-toggle-btn');
    btn.disabled = true;
    const restoreText = withLoadingText(btn, 'Saving…');
    try {
      const res = await fetch('/api/v1/output-pruning', {
        method: 'PUT', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id: pruningCurrentOrg, enabled: !pruningCurrentEnabled }),
      });
      const data = await res.json();
      if (!res.ok) { pruningError.textContent = data.error || 'Could not save.'; pruningError.style.display = 'block'; return; }
      pruningCurrentEnabled = !pruningCurrentEnabled;
      renderPruningStatus();
    } catch (err) { pruningError.textContent = 'Could not reach the server.'; pruningError.style.display = 'block'; }
    finally { btn.disabled = false; restoreText(); }
  });

  // ─── HITL Rules modal ───
  const hitlModalOverlay = document.getElementById('hitl-modal-overlay');
  const hitlForm = document.getElementById('hitl-form');
  const hitlError = document.getElementById('hitl-error');
  const hitlSubmit = document.getElementById('hitl-submit');

  document.getElementById('hitl-rules-btn').addEventListener('click', () => {
    hitlError.style.display = 'none'; hitlForm.reset();
    hitlModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('hitl-org').focus();
    loadHitlRulesList();
  });
  document.getElementById('hitl-modal-close').addEventListener('click', () => { hitlModalOverlay.style.display = 'none'; anyModalOpen = false; });
  hitlModalOverlay.addEventListener('click', (e) => { if (e.target === hitlModalOverlay) { hitlModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  hitlForm.addEventListener('submit', async (e) => {
    e.preventDefault(); hitlError.style.display = 'none'; hitlSubmit.disabled = true;
    const restoreText = withLoadingText(hitlSubmit, 'Saving…');
    const org_id = document.getElementById('hitl-org').value.trim();
    const service = document.getElementById('hitl-service').value.trim();
    const action = document.getElementById('hitl-action').value.trim();
    const field = document.getElementById('hitl-field').value.trim() || null;
    const operator = document.getElementById('hitl-operator').value || null;
    const thresholdRaw = document.getElementById('hitl-threshold').value.trim();
    const threshold = thresholdRaw === '' ? null : Number(thresholdRaw);
    try {
      const res = await fetch('/api/v1/hitl-rules', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, service, action, field, operator, threshold }),
      });
      const data = await res.json();
      if (!res.ok) { hitlError.textContent = data.error || 'Could not save rule.'; hitlError.style.display = 'block'; return; }
      hitlForm.reset(); loadHitlRulesList();
    } catch (err) { hitlError.textContent = 'Could not reach the server.'; hitlError.style.display = 'block'; }
    finally { hitlSubmit.disabled = false; restoreText(); }
  });

  async function loadHitlRulesList() {
    const list = document.getElementById('hitl-list');
    try {
      const res = await fetch('/api/v1/hitl-rules', { credentials: 'include' });
      if (!res.ok) return;
      const data = await res.json();
      const rows = data.rules || [];
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No HITL rules yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const condition = row.field ? `when ${escapeHtml(row.field)} ${escapeHtml(row.operator)} ${row.threshold}` : 'always';
        const div = document.createElement('div');
        div.className = 'rule-summary-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.service)}.${escapeHtml(row.action)} — ${escapeHtml(row.org_id)}</div><div class="meta">requires approval ${condition}</div></div><button class="revoke delete-hitl-rule" data-id="${row.id}">Delete</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.delete-hitl-rule').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/hitl-rules/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadHitlRulesList(); });
      });
    } catch (err) {}
  }

  // ─── Spend Cap Rules modal ───
  const spendcapModalOverlay = document.getElementById('spendcap-modal-overlay');
  const spendcapForm = document.getElementById('spendcap-form');
  const spendcapError = document.getElementById('spendcap-error');
  const spendcapSubmit = document.getElementById('spendcap-submit');

  document.getElementById('spendcap-rules-btn').addEventListener('click', () => {
    spendcapError.style.display = 'none'; spendcapForm.reset();
    spendcapModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('spendcap-org').focus();
    loadSpendcapRulesList();
  });
  document.getElementById('spendcap-modal-close').addEventListener('click', () => { spendcapModalOverlay.style.display = 'none'; anyModalOpen = false; });
  spendcapModalOverlay.addEventListener('click', (e) => { if (e.target === spendcapModalOverlay) { spendcapModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  spendcapForm.addEventListener('submit', async (e) => {
    e.preventDefault(); spendcapError.style.display = 'none'; spendcapSubmit.disabled = true;
    const restoreText = withLoadingText(spendcapSubmit, 'Saving…');
    const org_id = document.getElementById('spendcap-org').value.trim();
    const agent_id = document.getElementById('spendcap-agent').value.trim() || null;
    const service = document.getElementById('spendcap-service').value.trim();
    const action = document.getElementById('spendcap-action').value.trim();
    const capWindow = document.getElementById('spendcap-window').value;
    const max_calls = Number(document.getElementById('spendcap-max').value);
    const on_exceed = document.getElementById('spendcap-exceed').value;
    try {
      const res = await fetch('/api/v1/spend-cap-rules', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, agent_id, service, action, window: capWindow, max_calls, on_exceed }),
      });
      const data = await res.json();
      if (!res.ok) { spendcapError.textContent = data.error || 'Could not save rule.'; spendcapError.style.display = 'block'; return; }
      spendcapForm.reset(); loadSpendcapRulesList();
    } catch (err) { spendcapError.textContent = 'Could not reach the server.'; spendcapError.style.display = 'block'; }
    finally { spendcapSubmit.disabled = false; restoreText(); }
  });

  async function loadSpendcapRulesList() {
    const list = document.getElementById('spendcap-list');
    try {
      const res = await fetch('/api/v1/spend-cap-rules', { credentials: 'include' });
      if (!res.ok) return;
      const data = await res.json();
      const rows = data.rules || [];
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No spend-cap rules yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const scope = row.agent_id ? `agent ${escapeHtml(row.agent_id)}` : 'every agent';
        const exceedLabel = row.on_exceed === 'hitl' ? 'route to approval' : 'block';
        const div = document.createElement('div');
        div.className = 'rule-summary-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.service)}.${escapeHtml(row.action)} — ${escapeHtml(row.org_id)}</div><div class="meta">${row.max_calls}/${escapeHtml(row.window)} for ${scope}, then ${exceedLabel}</div></div><button class="revoke delete-spendcap-rule" data-id="${row.id}">Delete</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.delete-spendcap-rule').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/spend-cap-rules/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadSpendcapRulesList(); });
      });
    } catch (err) {}
  }

  // ─── Action Policies modal ───
  const policyModalOverlay = document.getElementById('policy-modal-overlay');
  const policyForm = document.getElementById('policy-form');
  const policyError = document.getElementById('policy-error');
  const policySubmit = document.getElementById('policy-submit');
  const policyEffect = document.getElementById('policy-effect');
  const showPolicyError = (msg) => { policyError.textContent = msg; policyError.style.display = 'block'; };
  const syncPolicyFields = () => { document.getElementById('policy-fields-wrap').style.display = policyEffect.value === 'require' ? '' : 'none'; };
  policyEffect.addEventListener('change', syncPolicyFields);

  document.getElementById('policy-rules-btn').addEventListener('click', () => {
    policyError.style.display = 'none'; policyForm.reset(); syncPolicyFields();
    policyModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('policy-org').focus();
    loadPolicyList();
  });
  document.getElementById('policy-modal-close').addEventListener('click', () => { policyModalOverlay.style.display = 'none'; anyModalOpen = false; });
  policyModalOverlay.addEventListener('click', (e) => { if (e.target === policyModalOverlay) { policyModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  policyForm.addEventListener('submit', async (e) => {
    e.preventDefault(); policyError.style.display = 'none';
    const effect = policyEffect.value;
    let fields = null;
    if (effect === 'require') {
      try { fields = JSON.parse(document.getElementById('policy-fields').value); }
      catch (err) { showPolicyError('Limits must be valid JSON, like {"amount": {"max": 50}}.'); return; }
    }
    policySubmit.disabled = true;
    const restoreText = withLoadingText(policySubmit, 'Saving…');
    try {
      const res = await fetch('/api/v1/action-policies', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({
          org_id: document.getElementById('policy-org').value.trim(),
          agent_id: document.getElementById('policy-agent').value.trim() || null,
          service: document.getElementById('policy-service').value.trim(),
          action: document.getElementById('policy-action').value.trim(),
          effect, fields,
          on_violation: document.getElementById('policy-violation').value,
        }),
      });
      const data = await res.json();
      if (!res.ok) { showPolicyError(data.error || 'Could not save policy.'); return; }
      policyForm.reset(); syncPolicyFields(); loadPolicyList();
    } catch (err) { showPolicyError('Could not reach the server.'); }
    finally { policySubmit.disabled = false; restoreText(); }
  });

  async function loadPolicyList() {
    const list = document.getElementById('policy-list');
    try {
      const res = await fetch('/api/v1/action-policies', { credentials: 'include' });
      if (!res.ok) return;
      const rows = (await res.json()).policies || [];
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No action policies yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const scope = row.agent_id ? `agent ${escapeHtml(row.agent_id)}` : 'every agent';
        const rule = row.effect === 'deny' ? 'never allowed' : `only when ${escapeHtml(JSON.stringify(row.fields))}`;
        const then = row.on_violation === 'hitl' ? 'route to approval' : 'block';
        const div = document.createElement('div');
        div.className = 'rule-summary-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.service)}.${escapeHtml(row.action)} (${escapeHtml(row.org_id)})</div><div class="meta">${scope}: ${rule}; otherwise ${then}</div></div><button class="revoke delete-policy" data-id="${Number(row.id)}">Delete</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.delete-policy').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/action-policies/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadPolicyList(); });
      });
    } catch (err) {}
  }

  // ─── Dev Tunnel inspector modal ───
  const tunnelModalOverlay = document.getElementById('tunnel-modal-overlay');
  const tunnelError = document.getElementById('tunnel-error');
  const tunnelList = document.getElementById('tunnel-list');
  let tunnelEventSource = null;

  function stopWatchingTunnel() {
    if (tunnelEventSource) { tunnelEventSource.close(); tunnelEventSource = null; }
  }

  document.getElementById('tunnel-btn').addEventListener('click', () => {
    tunnelError.style.display = 'none';
    tunnelModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('tunnel-org').focus();
  });
  document.getElementById('tunnel-modal-close').addEventListener('click', () => {
    stopWatchingTunnel();
    tunnelModalOverlay.style.display = 'none'; anyModalOpen = false;
  });
  tunnelModalOverlay.addEventListener('click', (e) => {
    if (e.target === tunnelModalOverlay) { stopWatchingTunnel(); tunnelModalOverlay.style.display = 'none'; anyModalOpen = false; }
  });

  document.getElementById('tunnel-watch-btn').addEventListener('click', () => {
    const orgId = document.getElementById('tunnel-org').value.trim();
    if (!orgId) { tunnelError.textContent = 'Enter an Org ID first.'; tunnelError.style.display = 'block'; return; }
    tunnelError.style.display = 'none';
    stopWatchingTunnel();
    tunnelList.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">Watching — waiting for requests…</div>';

    tunnelEventSource = new EventSource(`/api/v1/tunnel/stream?org_id=${encodeURIComponent(orgId)}`);
    let firstEntry = true;
    tunnelEventSource.onmessage = (ev) => {
      let entry;
      try { entry = JSON.parse(ev.data); } catch { return; }
      if (firstEntry) { tunnelList.innerHTML = ''; firstEntry = false; }
      const div = document.createElement('div');
      div.className = 'rule-summary-row';
      const time = entry.at ? new Date(entry.at).toLocaleTimeString() : '';
      div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(entry.method || '')} ${escapeHtml(entry.path || '')} → ${escapeHtml(String(entry.status || ''))}</div><div class="meta">${escapeHtml(time)}</div></div>`;
      tunnelList.prepend(div);
    };
    tunnelEventSource.onerror = () => {
      tunnelError.textContent = 'No active tunnel for this org (or it disconnected) — open one from your terminal first.';
      tunnelError.style.display = 'block';
      stopWatchingTunnel();
    };
  });

  // ─── Plugins modal (Agentgateway) ───
  const pluginsModalOverlay = document.getElementById('plugins-modal-overlay');
  const agentgatewayForm = document.getElementById('agentgateway-form');
  const agentgatewayError = document.getElementById('agentgateway-error');
  const agentgatewaySubmit = document.getElementById('agentgateway-submit');

  document.getElementById('plugins-btn').addEventListener('click', () => {
    agentgatewayError.style.display = 'none'; agentgatewayForm.reset();
    pluginsModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('agentgateway-org').focus();
    loadAgentgatewayList();
  });
  document.getElementById('plugins-modal-close').addEventListener('click', () => { pluginsModalOverlay.style.display = 'none'; anyModalOpen = false; });
  pluginsModalOverlay.addEventListener('click', (e) => { if (e.target === pluginsModalOverlay) { pluginsModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  agentgatewayForm.addEventListener('submit', async (e) => {
    e.preventDefault(); agentgatewayError.style.display = 'none'; agentgatewaySubmit.disabled = true;
    const restoreText = withLoadingText(agentgatewaySubmit, 'Saving…');
    const org_id = document.getElementById('agentgateway-org').value.trim();
    const name = document.getElementById('agentgateway-name').value.trim();
    const target_url = document.getElementById('agentgateway-url').value.trim();
    try {
      const res = await fetch('/api/v1/agentgateway-targets', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, name, target_url }),
      });
      const data = await res.json();
      if (!res.ok) { agentgatewayError.textContent = data.error || 'Could not save target.'; agentgatewayError.style.display = 'block'; return; }
      agentgatewayForm.reset(); loadAgentgatewayList();
    } catch (err) { agentgatewayError.textContent = 'Could not reach the server.'; agentgatewayError.style.display = 'block'; }
    finally { agentgatewaySubmit.disabled = false; restoreText(); }
  });

  async function loadAgentgatewayList() {
    const list = document.getElementById('agentgateway-list');
    try {
      const res = await fetch('/api/v1/agentgateway-targets', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No targets yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const div = document.createElement('div');
        div.className = 'rule-summary-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.name)} — ${escapeHtml(row.org_id)}</div><div class="meta">${escapeHtml(row.target_url)}</div></div><button class="revoke delete-agentgateway-target" data-id="${row.id}">Delete</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.delete-agentgateway-target').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/agentgateway-targets/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadAgentgatewayList(); });
      });
    } catch (err) {}
  }

  // ─── Outage Alerts (notification webhooks) modal ───
  const nwModalOverlay = document.getElementById('nw-modal-overlay');
  const nwForm = document.getElementById('nw-form');
  const nwError = document.getElementById('nw-error');
  const nwSubmit = document.getElementById('nw-submit');
  const nwType = document.getElementById('nw-type');

  function updateNwFieldVisibility() {
    const isTelegram = nwType.value === 'telegram';
    document.getElementById('nw-chatid-field').style.display = isTelegram ? 'block' : 'none';
    document.getElementById('nw-target-label').textContent = isTelegram ? 'Bot token' : 'Webhook URL';
    document.getElementById('nw-target').placeholder = isTelegram ? '123456:ABC-DEF...' : 'https://hooks.slack.com/services/...';
  }
  nwType.addEventListener('change', updateNwFieldVisibility);

  document.getElementById('notification-webhooks-btn').addEventListener('click', () => {
    nwError.style.display = 'none'; nwForm.reset(); updateNwFieldVisibility();
    nwModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('nw-org').focus();
    loadNotificationWebhooksList();
  });
  document.getElementById('nw-modal-close').addEventListener('click', () => { nwModalOverlay.style.display = 'none'; anyModalOpen = false; });
  nwModalOverlay.addEventListener('click', (e) => { if (e.target === nwModalOverlay) { nwModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  nwForm.addEventListener('submit', async (e) => {
    e.preventDefault(); nwError.style.display = 'none'; nwSubmit.disabled = true;
    const restoreText = withLoadingText(nwSubmit, 'Saving…');
    const org_id = document.getElementById('nw-org').value.trim();
    const type = nwType.value;
    const target = document.getElementById('nw-target').value.trim();
    const chat_id = document.getElementById('nw-chatid').value.trim();
    try {
      const res = await fetch('/api/v1/notification-webhooks', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, type, target, chat_id: chat_id || undefined }),
      });
      const data = await res.json();
      if (!res.ok) { nwError.textContent = data.error || 'Could not save.'; nwError.style.display = 'block'; return; }
      nwForm.reset(); updateNwFieldVisibility(); loadNotificationWebhooksList();
    } catch (err) { nwError.textContent = 'Could not reach the server.'; nwError.style.display = 'block'; }
    finally { nwSubmit.disabled = false; restoreText(); }
  });

  async function loadNotificationWebhooksList() {
    const list = document.getElementById('nw-list');
    try {
      const res = await fetch('/api/v1/notification-webhooks', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No outage alerts configured yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.type)} — ${escapeHtml(row.org_id)}</div><div class="meta">Added ${new Date(row.created_at).toLocaleDateString()}</div></div><div style="display:flex;gap:6px;"><button class="btn test-webhook" data-id="${row.id}" style="padding:6px 12px;font-size:12px;">Test</button><button class="revoke delete-webhook" data-id="${row.id}">Delete</button></div>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.test-webhook').forEach((btn) => {
        btn.addEventListener('click', async () => {
          const restoreText = withLoadingText(btn, 'Sending…');
          try { await fetch(`/api/v1/notification-webhooks/${btn.dataset.id}/test`, { method: 'POST', credentials: 'include' }); } finally { restoreText(); }
        });
      });
      list.querySelectorAll('.delete-webhook').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/notification-webhooks/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadNotificationWebhooksList(); });
      });
    } catch (err) {}
  }

  // ─── Failed requests (dead letter queue / one-click replay) modal ───
  const dlqModalOverlay = document.getElementById('dlq-modal-overlay');
  const dlqError = document.getElementById('dlq-error');

  document.getElementById('dlq-btn').addEventListener('click', () => {
    dlqError.style.display = 'none';
    dlqModalOverlay.style.display = 'flex'; anyModalOpen = true;
    loadDlqList();
  });
  document.getElementById('dlq-modal-close').addEventListener('click', () => { dlqModalOverlay.style.display = 'none'; anyModalOpen = false; });
  dlqModalOverlay.addEventListener('click', (e) => { if (e.target === dlqModalOverlay) { dlqModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  async function loadDlqList() {
    const list = document.getElementById('dlq-list');
    try {
      const res = await fetch('/api/v1/dead-letter-queue', { credentials: 'include' });
      if (!res.ok) return;
      const rows = await res.json();
      if (!rows.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No failed requests — nothing waiting to be replayed.</div>'; return; }
      list.innerHTML = '';
      for (const row of rows) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(row.service)}.${escapeHtml(row.action)} — ${escapeHtml(row.org_id)}</div><div class="meta">${escapeHtml(row.error_message || 'Upstream error')} · ${new Date(row.created_at).toLocaleString()}</div></div><div style="display:flex;gap:6px;"><button class="btn dlq-edit" data-id="${row.id}" style="padding:6px 12px;font-size:12px;">Edit &amp; Replay</button><button class="btn dlq-replay" data-id="${row.id}" style="padding:6px 12px;font-size:12px;">Replay</button><button class="revoke dlq-dismiss" data-id="${row.id}">Dismiss</button></div>`;
        list.appendChild(div);

        const editRow = document.createElement('div');
        editRow.id = `dlq-edit-${row.id}`;
        editRow.style.cssText = 'display:none;padding:0 0 14px;';
        editRow.innerHTML = `<textarea class="dlq-payload-edit" data-id="${row.id}" rows="6" style="width:100%;background:var(--panel-2);border:1px solid var(--border);border-radius:8px;color:var(--text);padding:10px 12px;font-family:ui-monospace,monospace;font-size:12.5px;resize:vertical;margin-bottom:8px;">${escapeHtml(JSON.stringify(row.payload, null, 2))}</textarea><button class="btn primary dlq-replay-edited" data-id="${row.id}" style="padding:6px 12px;font-size:12px;">Replay edited payload</button>`;
        list.appendChild(editRow);
      }

      async function doReplay(id, body) {
        dlqError.style.display = 'none';
        try {
          const res = await fetch(`/api/v1/dead-letter-queue/${id}/replay`, {
            method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
            body: JSON.stringify(body || {}),
          });
          const data = await res.json();
          if (!res.ok || !data.replayed) { dlqError.textContent = data.error || 'Replay failed.'; dlqError.style.display = 'block'; return; }
          loadDlqList();
        } catch (err) { dlqError.textContent = 'Could not reach the server.'; dlqError.style.display = 'block'; }
      }

      list.querySelectorAll('.dlq-edit').forEach((btn) => {
        btn.addEventListener('click', () => {
          const editRow = document.getElementById(`dlq-edit-${btn.dataset.id}`);
          editRow.style.display = editRow.style.display === 'none' ? 'block' : 'none';
        });
      });
      list.querySelectorAll('.dlq-replay').forEach((btn) => {
        btn.addEventListener('click', async () => {
          const restoreText = withLoadingText(btn, 'Replaying…');
          try { await doReplay(btn.dataset.id); } finally { restoreText(); }
        });
      });
      list.querySelectorAll('.dlq-replay-edited').forEach((btn) => {
        btn.addEventListener('click', async () => {
          const textarea = list.querySelector(`.dlq-payload-edit[data-id="${btn.dataset.id}"]`);
          let payload;
          try { payload = JSON.parse(textarea.value); }
          catch (err) { dlqError.textContent = 'Edited payload is not valid JSON.'; dlqError.style.display = 'block'; return; }
          const restoreText = withLoadingText(btn, 'Replaying…');
          try { await doReplay(btn.dataset.id, { payload }); } finally { restoreText(); }
        });
      });
      list.querySelectorAll('.dlq-dismiss').forEach((btn) => {
        btn.addEventListener('click', async () => { await fetch(`/api/v1/dead-letter-queue/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' }); loadDlqList(); });
      });
    } catch (err) {}
  }

  // ─── Reliability report (uptime %, success rate, duplicates prevented) ───
  const reliabilityModalOverlay = document.getElementById('reliability-modal-overlay');
  document.getElementById('reliability-btn').addEventListener('click', () => {
    reliabilityModalOverlay.style.display = 'flex'; anyModalOpen = true;
    loadReliabilityReport();
  });
  document.getElementById('reliability-modal-close').addEventListener('click', () => { reliabilityModalOverlay.style.display = 'none'; anyModalOpen = false; });
  reliabilityModalOverlay.addEventListener('click', (e) => { if (e.target === reliabilityModalOverlay) { reliabilityModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  async function loadReliabilityReport() {
    const list = document.getElementById('reliability-list');
    try {
      const res = await fetch(`/api/v1/reliability-report?range=${currentRange}`, { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) return;
      const data = await res.json();
      if (!data.services.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No services configured.</div>'; return; }
      list.innerHTML = '';
      for (const svc of data.services) {
        const uptimeColor = svc.uptime_pct >= 99.9 ? 'var(--signal)' : svc.uptime_pct >= 99 ? '#9A5B00' : 'var(--alert)';
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${svc.service.charAt(0).toUpperCase() + svc.service.slice(1)}</div><div class="meta">${svc.total_actions} action${svc.total_actions === 1 ? '' : 's'}${svc.success_rate !== null ? ` · ${svc.success_rate}% success` : ''}${svc.duplicates_prevented > 0 ? ` · ${svc.duplicates_prevented} duplicate${svc.duplicates_prevented === 1 ? '' : 's'} prevented` : ''}</div></div><div style="font-family:'Archivo',sans-serif;font-weight:700;font-size:15px;color:${uptimeColor};">${svc.uptime_pct}% uptime</div>`;
        list.appendChild(div);
      }
    } catch (err) {}
  }

  // ─── Run detail (fetches the complete step list for one run_id — the
  // activity badge above only filters whatever's already loaded into the
  // top-N recent-activity window, which can miss earlier steps of a run
  // once enough unrelated org-wide traffic pushes them out; this hits
  // /api/v1/dashboard/runs/:run_id directly, so it's always complete) ───
  const runModalOverlay = document.getElementById('run-modal-overlay');
  document.getElementById('run-modal-close').addEventListener('click', () => { runModalOverlay.style.display = 'none'; anyModalOpen = false; });
  runModalOverlay.addEventListener('click', (e) => { if (e.target === runModalOverlay) { runModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  async function openRunModal(runId) {
    document.getElementById('run-modal-sub').textContent = `run_id: ${runId}`;
    document.getElementById('run-modal-error').style.display = 'none';
    document.getElementById('run-modal-list').innerHTML = '';
    runModalOverlay.style.display = 'flex'; anyModalOpen = true;
    try {
      const res = await fetch(`/api/v1/dashboard/runs/${encodeURIComponent(runId)}`, { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) { document.getElementById('run-modal-error').style.display = 'block'; return; }
      const data = await res.json();
      const list = document.getElementById('run-modal-list');
      for (const step of data.steps) {
        const div = document.createElement('div');
        div.className = `log-row ${step.status}`;
        div.innerHTML = `
          <span class="time">${timeOnly(step.created_at)}</span>
          <span class="desc">${escapeHtml(step.service)}.${escapeHtml(step.action)}${step.step_id ? ` <span class="agent">— ${escapeHtml(step.step_id)}</span>` : ''}${step.error_type ? ` <span class="agent">(${escapeHtml(step.error_type)})</span>` : ''}</span>
          <span class="dur">${formatDuration(step.duration_ms)}</span>
          <span class="status-word">${step.status}</span>`;
        list.appendChild(div);
      }
    } catch (err) { document.getElementById('run-modal-error').style.display = 'block'; }
  }

  // ─── Active monitoring (proactive, opt-in health checks) ───
  const hcModalOverlay = document.getElementById('hc-modal-overlay');
  const hcForm = document.getElementById('hc-form');
  const hcError = document.getElementById('hc-error');
  const hcSubmit = document.getElementById('hc-submit');
  const hcServiceSelect = document.getElementById('hc-service');

  document.getElementById('health-checks-btn').addEventListener('click', () => {
    hcError.style.display = 'none'; hcForm.reset();
    hcModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('hc-org').focus();
    loadHealthChecksList();
  });
  document.getElementById('hc-modal-close').addEventListener('click', () => { hcModalOverlay.style.display = 'none'; anyModalOpen = false; });
  hcModalOverlay.addEventListener('click', (e) => { if (e.target === hcModalOverlay) { hcModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  hcForm.addEventListener('submit', async (e) => {
    e.preventDefault(); hcError.style.display = 'none'; hcSubmit.disabled = true;
    const restoreText = withLoadingText(hcSubmit, 'Enabling…');
    const org_id = document.getElementById('hc-org').value.trim();
    const service = hcServiceSelect.value;
    try {
      const res = await fetch('/api/v1/health-checks', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ org_id, service }),
      });
      const data = await res.json();
      if (!res.ok) { hcError.textContent = data.error || 'Could not enable monitoring.'; hcError.style.display = 'block'; return; }
      hcForm.reset(); loadHealthChecksList();
    } catch (err) { hcError.textContent = 'Could not reach the server.'; hcError.style.display = 'block'; }
    finally { hcSubmit.disabled = false; restoreText(); }
  });

  async function loadHealthChecksList() {
    const list = document.getElementById('hc-list');
    try {
      const res = await fetch('/api/v1/health-checks', { credentials: 'include' });
      if (res.status === 401) return handleUnauthenticated();
      if (!res.ok) return;
      const data = await res.json();
      hcServiceSelect.innerHTML = data.supported_services.map((s) => `<option value="${s}">${s.charAt(0).toUpperCase() + s.slice(1)}</option>`).join('');
      if (!data.enabled.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">Nothing actively monitored yet.</div>'; return; }
      list.innerHTML = '';
      for (const row of data.enabled) {
        const statusText = row.last_checked_at
          ? (row.last_ok ? `Passing (${row.last_latency_ms}ms) · ${new Date(row.last_checked_at).toLocaleString()}` : `Failing: ${escapeHtml(row.last_error || 'unknown error')} · ${new Date(row.last_checked_at).toLocaleString()}`)
          : 'Waiting for first check…';
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc"><span class="heartbeat ${row.last_checked_at && !row.last_ok ? 'down' : ''}"></span> ${escapeHtml(row.service)} — ${escapeHtml(row.org_id)}</div><div class="meta">${statusText}</div></div><button class="revoke hc-disable" data-org="${row.org_id}" data-service="${row.service}">Disable</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.hc-disable').forEach((btn) => {
        btn.addEventListener('click', async () => {
          await fetch(`/api/v1/health-checks/${encodeURIComponent(btn.dataset.org)}/${encodeURIComponent(btn.dataset.service)}`, { method: 'DELETE', credentials: 'include' });
          loadHealthChecksList();
        });
      });
    } catch (err) {}
  }

  // ─── Enterprise modal (SSO configs, members, invites) ───
  const enterpriseModalOverlay = document.getElementById('enterprise-modal-overlay');
  const enterpriseError = document.getElementById('enterprise-error');
  const enterpriseOrgInput = document.getElementById('enterprise-org-input');
  let enterpriseCurrentOrg = '';
  let enterpriseMemberCount = 0;
  let enterpriseInviteCount = 0;

  function enterpriseShowError(msg) { enterpriseError.textContent = msg; enterpriseError.style.display = 'block'; }
  function roleBadge(role) {
    const cls = role === 'admin' ? 'admin-role' : role === 'auditor' ? 'role-auditor' : 'role-developer';
    return `<span class="badge ${cls}">${escapeHtml(role)}</span>`;
  }

  document.getElementById('enterprise-btn').addEventListener('click', () => {
    enterpriseError.style.display = 'none';
    document.getElementById('enterprise-configs-list').innerHTML = '';
    document.getElementById('enterprise-members-list').innerHTML = '';
    document.getElementById('enterprise-invites-list').innerHTML = '';
    enterpriseModalOverlay.style.display = 'flex'; anyModalOpen = true;
    enterpriseOrgInput.focus();
  });
  document.getElementById('enterprise-modal-close').addEventListener('click', () => { enterpriseModalOverlay.style.display = 'none'; anyModalOpen = false; });
  enterpriseModalOverlay.addEventListener('click', (e) => { if (e.target === enterpriseModalOverlay) { enterpriseModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  document.getElementById('enterprise-load-btn').addEventListener('click', () => loadEnterpriseOrg());
  enterpriseOrgInput.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); loadEnterpriseOrg(); } });

  async function loadEnterpriseOrg() {
    const orgId = enterpriseOrgInput.value.trim();
    if (!orgId) { enterpriseShowError('Enter an org ID first.'); return; }
    enterpriseCurrentOrg = orgId;
    enterpriseError.style.display = 'none';
    await Promise.all([loadEnterpriseConfigs(), loadEnterpriseMembers(), loadEnterpriseInvites()]);
    await updateSeatUsage();
  }

  // Seat usage is member + pending-invite count against the org's tier
  // limit (Community=1, Team=3, Enterprise=unlimited) — mirrors
  // agentraas_core::tier::Tier::seat_limit, the same limit require_tier
  // enforces server-side on invite creation.
  async function updateSeatUsage() {
    const el = document.getElementById('enterprise-seat-usage');
    el.textContent = '';
    if (!enterpriseCurrentOrg) return;
    const used = enterpriseMemberCount + enterpriseInviteCount;
    el.textContent = `${used} seats used (unlimited).`;
  }

  async function loadEnterpriseConfigs() {
    const list = document.getElementById('enterprise-configs-list');
    try {
      const res = await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/configs`, { credentials: 'include' });
      const data = await res.json();
      if (!res.ok) { list.innerHTML = ''; enterpriseShowError(data.error || 'Could not load SSO configs.'); return; }
      const configs = data.configs || [];
      if (!configs.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No identity providers configured yet.</div>'; return; }
      list.innerHTML = '';
      for (const c of configs) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(c.client_id)} ${c.enabled ? '' : '<span class="badge unverified">disabled</span>'}</div><div class="meta">${escapeHtml(c.issuer_url)} · domains: ${escapeHtml(c.allowed_domains)} · secret ${escapeHtml(c.client_secret_preview)} · default role: ${escapeHtml(c.default_role)}</div></div><button class="revoke" data-id="${c.id}">Delete</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.revoke').forEach((btn) => {
        btn.addEventListener('click', async () => {
          await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/configs/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' });
          loadEnterpriseConfigs();
        });
      });
    } catch (err) { enterpriseShowError('Could not reach the server.'); }
  }

  const enterpriseConfigForm = document.getElementById('enterprise-config-form');
  const enterpriseConfigSubmit = document.getElementById('enterprise-config-submit');
  enterpriseConfigForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    if (!enterpriseCurrentOrg) { enterpriseShowError('Load an org first.'); return; }
    enterpriseError.style.display = 'none'; enterpriseConfigSubmit.disabled = true;
    const restoreText = withLoadingText(enterpriseConfigSubmit, 'Saving…');
    const body = {
      issuer_url: document.getElementById('config-issuer').value.trim(),
      client_id: document.getElementById('config-client-id').value.trim(),
      client_secret: document.getElementById('config-client-secret').value.trim(),
      allowed_domains: document.getElementById('config-domains').value.trim(),
      default_role: document.getElementById('config-default-role').value,
    };
    try {
      const res = await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/configs`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify(body),
      });
      const data = await res.json();
      if (!res.ok) { enterpriseShowError(data.error || 'Could not save identity provider.'); return; }
      enterpriseConfigForm.reset(); loadEnterpriseConfigs();
    } catch (err) { enterpriseShowError('Could not reach the server.'); }
    finally { enterpriseConfigSubmit.disabled = false; restoreText(); }
  });

  async function loadEnterpriseMembers() {
    const list = document.getElementById('enterprise-members-list');
    try {
      const res = await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/members`, { credentials: 'include' });
      const data = await res.json();
      if (!res.ok) { list.innerHTML = ''; enterpriseMemberCount = 0; return; } // 403 here is normal for a non-admin viewer; the configs call above already surfaced the error
      const members = data.members || [];
      enterpriseMemberCount = members.length;
      if (!members.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No members yet.</div>'; return; }
      list.innerHTML = '';
      for (const m of members) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(m.email)} ${roleBadge(m.role)}</div></div>
          <div class="sso-row-actions">
            <select data-user="${m.user_id}" class="member-role-select">
              <option value="admin" ${m.role === 'admin' ? 'selected' : ''}>Admin</option>
              <option value="developer" ${m.role === 'developer' ? 'selected' : ''}>Developer</option>
              <option value="auditor" ${m.role === 'auditor' ? 'selected' : ''}>Auditor</option>
            </select>
            <button class="revoke" data-user="${m.user_id}">Remove</button>
          </div>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.member-role-select').forEach((sel) => {
        sel.addEventListener('change', async () => {
          await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/members/${sel.dataset.user}`, {
            method: 'PUT', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify({ role: sel.value }),
          });
          loadEnterpriseMembers();
        });
      });
      list.querySelectorAll('.revoke').forEach((btn) => {
        btn.addEventListener('click', async () => {
          await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/members/${btn.dataset.user}`, { method: 'DELETE', credentials: 'include' });
          await loadEnterpriseMembers(); updateSeatUsage();
        });
      });
    } catch (err) {}
  }

  async function loadEnterpriseInvites() {
    const list = document.getElementById('enterprise-invites-list');
    try {
      const res = await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/invites`, { credentials: 'include' });
      const data = await res.json();
      if (!res.ok) { list.innerHTML = ''; enterpriseInviteCount = 0; return; }
      const invites = data.invites || [];
      enterpriseInviteCount = invites.length;
      if (!invites.length) { list.innerHTML = '<div style="color:var(--muted);font-size:13px;padding:8px 0;">No pending invites.</div>'; return; }
      list.innerHTML = '';
      for (const inv of invites) {
        const div = document.createElement('div');
        div.className = 'cred-row';
        div.innerHTML = `<div class="info"><div class="svc">${escapeHtml(inv.email)} ${roleBadge(inv.role)}</div><div class="meta">expires ${new Date(inv.expires_at).toLocaleDateString()}</div></div><button class="revoke" data-id="${inv.id}">Revoke</button>`;
        list.appendChild(div);
      }
      list.querySelectorAll('.revoke').forEach((btn) => {
        btn.addEventListener('click', async () => {
          await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/invites/${btn.dataset.id}`, { method: 'DELETE', credentials: 'include' });
          await loadEnterpriseInvites(); updateSeatUsage();
        });
      });
    } catch (err) {}
  }

  const enterpriseInviteForm = document.getElementById('enterprise-invite-form');
  const enterpriseInviteSubmit = document.getElementById('enterprise-invite-submit');
  enterpriseInviteForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    if (!enterpriseCurrentOrg) { enterpriseShowError('Load an org first.'); return; }
    enterpriseError.style.display = 'none'; enterpriseInviteSubmit.disabled = true;
    const restoreText = withLoadingText(enterpriseInviteSubmit, 'Sending…');
    const body = { email: document.getElementById('invite-email-input').value.trim(), role: document.getElementById('invite-role-select').value };
    try {
      const res = await fetch(`/api/v1/auth/sso/${encodeURIComponent(enterpriseCurrentOrg)}/invites`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify(body),
      });
      const data = await res.json();
      if (!res.ok) { enterpriseShowError(data.error || 'Could not send invite.'); return; }
      enterpriseInviteForm.reset(); await loadEnterpriseInvites(); updateSeatUsage();
      if (data.dev_accept_url) { enterpriseShowError(`Dev mode (no SMTP configured) — accept link: ${data.dev_accept_url}`); }
    } catch (err) { enterpriseShowError('Could not reach the server.'); }
    finally { enterpriseInviteSubmit.disabled = false; restoreText(); }
  });

  // ─── SSO sign-in link on the login screen ───
  const ssoOrgForm = document.getElementById('sso-org-form');
  document.getElementById('sso-link').addEventListener('click', () => {
    ssoOrgForm.style.display = ssoOrgForm.style.display === 'none' ? 'block' : 'none';
    if (ssoOrgForm.style.display === 'block') document.getElementById('sso-org-input').focus();
  });
  ssoOrgForm.addEventListener('submit', (e) => {
    e.preventDefault();
    const orgId = document.getElementById('sso-org-input').value.trim();
    if (!orgId) return;
    window.location.href = `/api/v1/auth/sso/${encodeURIComponent(orgId)}/login`;
  });

  // ─── Accept an org invite ───
  const inviteScreen = document.getElementById('invite-screen');
  const inviteForm = document.getElementById('invite-form');
  const inviteError = document.getElementById('invite-error');
  const inviteSubmit = document.getElementById('invite-submit');
  let pendingInviteToken = null;
  inviteForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    inviteError.style.display = 'none'; inviteSubmit.disabled = true;
    const restoreText = withLoadingText(inviteSubmit, 'Joining…');
    const password = document.getElementById('invite-password-input').value;
    try {
      const res = await fetch('/api/v1/auth/invites/accept', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ token: pendingInviteToken, password: password || undefined }),
      });
      const data = await res.json();
      if (!res.ok) { inviteError.textContent = data.error || 'Could not accept this invite.'; inviteError.style.display = 'block'; return; }
      window.location.href = '/dashboard';
    } catch (err) { inviteError.textContent = 'Could not reach the server.'; inviteError.style.display = 'block'; }
    finally { inviteSubmit.disabled = false; restoreText(); }
  });

  // ─── Account modal (change password) ───
  const accountModalOverlay = document.getElementById('account-modal-overlay');
  const accountForm = document.getElementById('account-form');
  const accountError = document.getElementById('account-error');
  const accountSuccess = document.getElementById('account-success');
  const accountSubmit = document.getElementById('account-submit');

  accountBtn.addEventListener('click', async () => {
    document.getElementById('account-email-display').textContent = currentUserEmail;
    accountError.style.display = 'none'; accountSuccess.style.display = 'none'; accountForm.reset();
    accountModalOverlay.style.display = 'flex'; anyModalOpen = true;
    document.getElementById('current-password').focus();
    await loadSelfHostGateState();
    await loadBillingPlanState();
  });
  document.getElementById('account-modal-close').addEventListener('click', () => { accountModalOverlay.style.display = 'none'; anyModalOpen = false; });
  accountModalOverlay.addEventListener('click', (e) => { if (e.target === accountModalOverlay) { accountModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  // ─── Self-host download gate: must have connected an agent, then submit a
  // short request form before the actual download unlocks ───
  async function loadSelfHostGateState() {
    const loadingEl = document.getElementById('self-host-loading');
    const noAgentEl = document.getElementById('self-host-no-agent');
    const formEl = document.getElementById('self-host-request-form');
    loadingEl.style.display = 'block'; noAgentEl.style.display = 'none'; formEl.style.display = 'none';
    try {
      const res = await fetch('/api/v1/agents/keys', { credentials: 'include' });
      const keys = await res.json();
      loadingEl.style.display = 'none';
      if (Array.isArray(keys) && keys.length > 0) {
        formEl.style.display = 'block';
      } else {
        noAgentEl.style.display = 'block';
      }
    } catch (err) {
      loadingEl.textContent = 'Could not check status — try again.';
    }
  }

  // ─── Billing ───
  // Mirrors agentraas_core::tier::Tier::seat_limit — null means unlimited.
  // "pro"/"agency" kept as aliases for a self-hosted instance whose
  // users.plan was set by hand before the tier rename (Pro -> Team,
  // Agency folded into Enterprise) — same aliasing Tier::from_plan_str
  // does server-side.
  // textContent only, never innerHTML.
  const ACCOUNT_DESCRIPTIONS = {
    free: 'Free Cloud account: every feature, 500 actions a month.',
    payg: 'Pay as you go: every feature, $1 per 1,000 actions after 500 free each month.',
    other: 'Every feature included.',
  };

  async function loadBillingPlanState() {
    const planEl = document.getElementById('billing-current-plan');
    const brandingSection = document.getElementById('branding-section');
    try {
      const res = await fetch('/api/v1/auth/me', { credentials: 'include' });
      const data = await res.json();
      const plan = data.user?.plan || 'free';
      planEl.textContent = ACCOUNT_DESCRIPTIONS[plan] || ACCOUNT_DESCRIPTIONS.other;
      brandingSection.style.display = 'block';
    } catch (err) {
      planEl.textContent = 'Could not load plan status.';
    }
    loadPaygState();
  }

  // Shown only once billing.rs is switched on (BILLING_PAYG_ENABLED), or
  // to an org already on pay as you go.
  async function loadPaygState() {
    const section = document.getElementById('payg-section');
    section.style.display = 'none';
    if (!currentUserOrgId) return;
    try {
      const res = await fetch(`/api/v1/billing/usage?org_id=${encodeURIComponent(currentUserOrgId)}`, { credentials: 'include' });
      if (!res.ok) return;
      const u = await res.json();
      if (!u.enabled && !u.payg) return;
      section.style.display = 'block';
      document.getElementById('payg-usage').textContent = u.payg
        ? `${u.month}: ${u.actions} actions ran (${u.free_actions} free). So far: $${(u.estimated_cents / 100).toFixed(2)}. Cap: $${u.monthly_cap_usd}.`
        : `${u.month}: ${u.actions} actions ran of ${u.free_actions} free.`;
      document.getElementById('payg-start-btn').style.display = u.payg ? 'none' : 'flex';
      document.getElementById('payg-cap-row').style.display = u.payg ? 'flex' : 'none';
      document.getElementById('payg-cap-input').value = u.monthly_cap_usd;
    } catch (err) {}
  }

  document.getElementById('payg-start-btn').addEventListener('click', () => {
    startCheckout('payg', document.getElementById('payg-start-btn'), document.getElementById('payg-error'));
  });

  document.getElementById('payg-cap-save').addEventListener('click', async () => {
    const errorBox = document.getElementById('payg-error');
    errorBox.style.display = 'none';
    const cap = parseInt(document.getElementById('payg-cap-input').value, 10);
    try {
      const res = await fetch('/api/v1/billing/usage', {
        method: 'PUT', credentials: 'include', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ org_id: currentUserOrgId, monthly_cap_usd: cap }),
      });
      const data = await res.json();
      if (!res.ok) { errorBox.textContent = data.error || 'Could not save the cap.'; errorBox.style.display = 'block'; return; }
      loadPaygState();
    } catch (err) {
      errorBox.textContent = 'Could not save the cap.'; errorBox.style.display = 'block';
    }
  });

  let paddleLoaded = false;
  function loadPaddleScript() {
    return new Promise((resolve, reject) => {
      if (paddleLoaded || window.Paddle) { paddleLoaded = true; resolve(); return; }
      const script = document.createElement('script');
      script.src = 'https://cdn.paddle.com/paddle/v2/paddle.js';
      script.onload = () => { paddleLoaded = true; resolve(); };
      script.onerror = () => reject(new Error('Could not load Paddle checkout.'));
      document.head.appendChild(script);
    });
  }

  // Shared checkout trigger - used by the header's quick-access button and
  // the Account modal's Team button, so the logic lives in one place. Team
  // is the only self-serve paid plan (Enterprise is sales-assisted).
  // errorBox is optional: pass an element to show errors inline, or omit
  // to fall back to alert() (used by the header button, which has no
  // nearby space for an inline error message).
  async function startCheckout(plan, btn, errorBox) {
    if (errorBox) errorBox.style.display = 'none';
    btn.disabled = true;
    const restoreText = withLoadingText(btn, 'Loading checkout...');
    try {
      const infoRes = await fetch(`/api/v1/billing/checkout-info?plan=${encodeURIComponent(plan)}`, { credentials: 'include' });
      const info = await infoRes.json();
      if (!infoRes.ok) {
        const msg = info.error || 'Billing is not available.';
        if (errorBox) { errorBox.textContent = msg; errorBox.style.display = 'block'; } else { alert(msg); }
        return;
      }
      await loadPaddleScript();
      Paddle.Environment.set(info.environment);
      Paddle.Initialize({ token: info.client_token });
      Paddle.Checkout.open({
        items: [{ priceId: info.price_id, quantity: 1 }],
        customer: { email: info.email },
        customData: info.custom_data,
      });
    } catch (err) {
      const msg = err.message || 'Could not start checkout.';
      if (errorBox) { errorBox.textContent = msg; errorBox.style.display = 'block'; } else { alert(msg); }
    } finally {
      btn.disabled = false; restoreText();
    }
  }



  document.getElementById('branding-save-btn').addEventListener('click', async () => {
    const orgId = document.getElementById('branding-org-input').value.trim();
    const errorBox = document.getElementById('branding-error');
    errorBox.style.display = 'none';
    if (!orgId) { errorBox.textContent = 'Enter an org ID.'; errorBox.style.display = 'block'; return; }
    const btn = document.getElementById('branding-save-btn');
    btn.disabled = true;
    const restoreText = withLoadingText(btn, 'Saving...');
    try {
      const res = await fetch(`/api/v1/org-branding/${encodeURIComponent(orgId)}`, {
        method: 'PUT', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({
          display_name: document.getElementById('branding-name-input').value.trim() || undefined,
          logo_url: document.getElementById('branding-logo-input').value.trim() || undefined,
        }),
      });
      const data = await res.json();
      if (!res.ok) { errorBox.textContent = data.error || 'Could not save branding.'; errorBox.style.display = 'block'; }
    } catch (err) {
      errorBox.textContent = 'Could not save branding.'; errorBox.style.display = 'block';
    } finally {
      btn.disabled = false; restoreText();
    }
  });

  document.getElementById('self-host-submit').addEventListener('click', async () => {
    const errorBox = document.getElementById('self-host-error');
    errorBox.style.display = 'none';
    const reason = document.getElementById('self-host-reason').value;
    const company = document.getElementById('self-host-company').value;
    const submitBtn = document.getElementById('self-host-submit');
    submitBtn.disabled = true;
    const restoreText = withLoadingText(submitBtn, 'Preparing download…');
    try {
      const res = await fetch('/api/v1/download/self-host/request', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include',
        body: JSON.stringify({ reason, company }),
      });
      const data = await res.json();
      if (!res.ok) { errorBox.textContent = data.error || 'Could not process request.'; errorBox.style.display = 'block'; return; }
      window.location.href = '/api/v1/download/self-host';
    } catch (err) {
      errorBox.textContent = 'Could not reach the server.'; errorBox.style.display = 'block';
    } finally {
      submitBtn.disabled = false; restoreText();
    }
  });

  // ─── Admin modal ───
  const adminModalOverlay = document.getElementById('admin-modal-overlay');
  let adminUsersCache = [];
  let adminActiveTab = 'local';

  document.getElementById('admin-btn').addEventListener('click', () => {
    adminModalOverlay.style.display = 'flex'; anyModalOpen = true;
    loadAdminData();
  });
  document.getElementById('admin-modal-close').addEventListener('click', () => { adminModalOverlay.style.display = 'none'; anyModalOpen = false; });
  adminModalOverlay.addEventListener('click', (e) => { if (e.target === adminModalOverlay) { adminModalOverlay.style.display = 'none'; anyModalOpen = false; } });

  document.querySelectorAll('.admin-tab').forEach((tab) => {
    tab.addEventListener('click', () => {
      adminActiveTab = tab.dataset.tab;
      document.querySelectorAll('.admin-tab').forEach((t) => t.classList.toggle('active', t === tab));
      renderAdminUsersList();
    });
  });

  async function loadAdminData() {
    try {
      const [overviewRes, usersRes] = await Promise.all([
        fetch('/api/v1/admin/overview', { credentials: 'include' }),
        fetch('/api/v1/admin/users', { credentials: 'include' }),
      ]);
      if (overviewRes.status === 403 || usersRes.status === 403) {
        document.getElementById('admin-overview-line').textContent = 'Admin access required.';
        document.getElementById('admin-stat-grid').style.display = 'none';
        return;
      }
      const overview = await overviewRes.json();
      adminUsersCache = await usersRes.json();

      document.getElementById('admin-stat-users').textContent = overview.total_users;
      document.getElementById('admin-stat-orgs').textContent = overview.total_orgs;
      document.getElementById('admin-stat-actions').textContent = overview.this_month.success || 0;
      document.getElementById('admin-overview-line').textContent = `Running in ${overview.deployment_mode} mode`;

      renderAdminUsersList();
    } catch (err) {}
  }

  function renderAdminUsersList() {
    const list = document.getElementById('admin-users-list');
    list.innerHTML = '';
    const users = adminUsersCache.filter((u) => {
      if (adminActiveTab === 'local') return u.id < 10;
      if (adminActiveTab === 'service') return u.id >= 10 && u.id < 100;
      return u.id >= 100;
    });

    if (!users.length) {
      list.innerHTML = `<div style="color:var(--muted);font-size:13px;padding:12px 0;">No ${adminActiveTab} users yet.</div>`;
      return;
    }

    for (const u of users) {
      const row = document.createElement('div');
      row.className = 'cred-row clickable';
      const lastLogin = u.last_login_at ? new Date(u.last_login_at).toLocaleDateString() : 'never';
      const orgsLabel = u.org_count === 0 ? 'no orgs yet' : `${u.org_count} org${u.org_count === 1 ? '' : 's'}`;
      const adminBadge = u.is_admin ? '<span class="badge admin-role">Admin</span>' : '';
      const planBadge = (u.plan === 'enterprise' || u.plan === 'agency') ? '<span class="badge role-developer">Enterprise</span>'
        : (u.plan === 'team' || u.plan === 'pro') ? '<span class="badge pro">Team</span>' : '';
      row.innerHTML = `
        <div class="info">
          <div class="svc"><span class="user-id-badge">#${u.id}</span>${escapeHtml(u.email)} ${adminBadge} ${planBadge}</div>
          <div class="meta">${orgsLabel} · ${u.usage_this_month} actions this month · last login ${lastLogin}</div>
        </div>`;
      list.appendChild(row);

      // Expandable detail row — every field the admin/users endpoint has.
      const detail = document.createElement('div');
      detail.className = 'user-detail-row';
      const orgsListHtml = u.orgs.length ? u.orgs.map(escapeHtml).join(', ') : 'none';
      const verifiedBadge = u.email_verified
        ? '<span class="badge verified">Verified</span>'
        : '<span class="badge unverified">Not verified</span>';
      detail.innerHTML = `
        <div><b>Email:</b> ${verifiedBadge}</div>
        <div><b>Created:</b> ${new Date(u.created_at).toLocaleString()}</div>
        <div><b>Last login:</b> ${lastLogin}</div>
        <div><b>Orgs:</b> ${orgsListHtml}</div>
        <div><b>API keys:</b> ${u.api_keys_count} · <b>Custom actions:</b> ${u.custom_actions_count} · <b>Saved credentials:</b> ${u.credentials_count}</div>
        <div><b>Usage this month:</b> ${u.usage_this_month} actions</div>`;
      list.appendChild(detail);

      row.querySelector('.info').addEventListener('click', () => detail.classList.toggle('open'));
    }
  }

  accountForm.addEventListener('submit', async (e) => {
    e.preventDefault();
    accountError.style.display = 'none'; accountSuccess.style.display = 'none'; accountSubmit.disabled = true;
    const current_password = document.getElementById('current-password').value;
    const new_password = document.getElementById('new-password').value;
    const confirm_password = document.getElementById('confirm-password').value;
    if (new_password !== confirm_password) {
      accountError.textContent = 'New password and confirmation don\'t match.'; accountError.style.display = 'block';
      accountSubmit.disabled = false; return;
    }
    const restoreText = withLoadingText(accountSubmit, 'Updating…');
    try {
      const res = await fetch('/api/v1/auth/password', { method: 'POST', headers: { 'Content-Type': 'application/json' }, credentials: 'include', body: JSON.stringify({ current_password, new_password }) });
      const data = await res.json();
      if (!res.ok) { accountError.textContent = data.error || 'Could not update password.'; accountError.style.display = 'block'; return; }
      accountSuccess.textContent = 'Password updated.'; accountSuccess.style.display = 'block'; accountForm.reset();
    } catch (err) { accountError.textContent = 'Could not reach the server.'; accountError.style.display = 'block'; }
    finally { accountSubmit.disabled = false; restoreText(); }
  });

  // ─── Escape closes whichever modal is open ───
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    [modalOverlay, credsModalOverlay, customModalOverlay, accountModalOverlay, adminModalOverlay].forEach((ov) => {
      if (ov.style.display === 'flex') { ov.style.display = 'none'; anyModalOpen = false; }
    });
  });

  // ─── Forgot password ───
  const forgotScreen = document.getElementById('forgot-screen');
  const forgotForm = document.getElementById('forgot-form');
  const forgotMessage = document.getElementById('forgot-message');
  const forgotSubmit = document.getElementById('forgot-submit');

  document.getElementById('forgot-link').addEventListener('click', () => {
    authScreen.style.display = 'none'; forgotScreen.style.display = 'block';
    forgotMessage.style.display = 'none'; forgotForm.reset();
  });
  document.getElementById('back-to-login-link').addEventListener('click', () => {
    forgotScreen.style.display = 'none'; authScreen.style.display = 'block';
  });

  forgotForm.addEventListener('submit', async (e) => {
    e.preventDefault(); forgotSubmit.disabled = true;
    const restoreText = withLoadingText(forgotSubmit, 'Sending…');
    const email = document.getElementById('forgot-email').value.trim();
    try {
      const res = await fetch('/api/v1/auth/forgot-password', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ email }),
      });
      const data = await res.json();
      forgotMessage.textContent = data.message || 'If an account exists for that email, a reset link has been sent.';
      forgotMessage.style.display = 'block';
      forgotForm.reset();
    } catch (err) {
      forgotMessage.textContent = 'Could not reach the server.';
      forgotMessage.style.display = 'block';
    } finally {
      forgotSubmit.disabled = false; restoreText();
    }
  });

  // ─── Reset password (from emailed link) ───
  const resetScreen = document.getElementById('reset-screen');
  const resetForm = document.getElementById('reset-form');
  const resetError = document.getElementById('reset-error');
  const resetSubmit = document.getElementById('reset-submit');
  let pendingResetToken = null;

  resetForm.addEventListener('submit', async (e) => {
    e.preventDefault(); resetError.style.display = 'none'; resetSubmit.disabled = true;
    const new_password = document.getElementById('reset-password-input').value;
    const confirm_password = document.getElementById('reset-confirm-input').value;
    if (new_password !== confirm_password) {
      resetError.textContent = 'Passwords don\'t match.'; resetError.style.display = 'block';
      resetSubmit.disabled = false; return;
    }
    const restoreText = withLoadingText(resetSubmit, 'Setting password…');
    try {
      const res = await fetch('/api/v1/auth/reset-password', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token: pendingResetToken, new_password }),
      });
      const data = await res.json();
      if (!res.ok) { resetError.textContent = data.error || 'Could not reset password.'; resetError.style.display = 'block'; return; }
      resetScreen.style.display = 'none'; authScreen.style.display = 'block';
      authError.textContent = ''; authError.style.display = 'none';
      // Clear the token from the URL so refreshing doesn't re-show the reset form.
      window.history.replaceState({}, '', window.location.pathname);
    } catch (err) {
      resetError.textContent = 'Could not reach the server.'; resetError.style.display = 'block';
    } finally {
      resetSubmit.disabled = false; restoreText();
    }
  });

  (async function checkSession() {
    // A password-reset link or email-verification link takes priority over
    // the normal login/session flow.
    const urlParams = new URLSearchParams(window.location.search);
    const resetToken = urlParams.get('reset_token');
    if (resetToken) {
      pendingResetToken = resetToken;
      resetScreen.style.display = 'block';
      return;
    }

    const inviteToken = urlParams.get('invite_token');
    if (inviteToken) {
      window.history.replaceState({}, '', window.location.pathname);
      pendingInviteToken = inviteToken;
      inviteScreen.style.display = 'block';
      return;
    }

    const verifyToken = urlParams.get('verify_token');
    if (verifyToken) {
      window.history.replaceState({}, '', window.location.pathname); // don't leave the token sitting in the URL/history
      try {
        const res = await fetch(`/api/v1/auth/verify-email?token=${encodeURIComponent(verifyToken)}`, { credentials: 'include' });
        const data = await res.json();
        if (res.ok) { showDashboard(data.user); return; }
        authError.textContent = data.error || 'Could not verify your email.'; authError.style.display = 'block';
      } catch (err) {
        authError.textContent = 'Could not reach the server.'; authError.style.display = 'block';
      }
      authScreen.style.display = 'block';
      return;
    }

    try {
      const res = await fetch('/api/v1/auth/me', { credentials: 'include' });
      if (res.ok) { const data = await res.json(); showDashboard(data.user); return; }
    } catch (err) {}
    authScreen.style.display = 'block';
  })();
})();
