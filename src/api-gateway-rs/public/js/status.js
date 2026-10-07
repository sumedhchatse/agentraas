(function () {
  const STATUS_LABEL = { operational: 'Operational', degraded: 'Degraded', down: 'Down' };

  function uptimeColor(pct) {
    if (pct >= 99.9) return 'var(--claim)';
    if (pct >= 99) return 'var(--warn)';
    return 'var(--alert)';
  }

  async function load() {
    try {
      const res = await fetch('/api/v1/public/status');
      if (!res.ok) throw new Error('bad status');
      const data = await res.json();

      const banner = document.getElementById('banner');
      banner.className = 'banner ' + data.overall;
      banner.style.display = 'flex';
      const titles = { operational: 'All systems operational', degraded: 'Degraded performance', major_outage: 'Active outage' };
      const subs = {
        operational: 'Every protected service is responding normally.',
        degraded: 'One or more services are recovering from a recent disruption.',
        major_outage: 'One or more services are currently unavailable.',
      };
      document.getElementById('banner-title').textContent = titles[data.overall] || 'Status';
      document.getElementById('banner-sub').textContent = subs[data.overall] || '';

      const list = document.getElementById('service-list');
      list.innerHTML = '';
      for (const svc of data.services) {
        const row = document.createElement('div');
        row.className = 'service-row';
        const color = uptimeColor(svc.uptime_90d);
        row.innerHTML = `
          <div class="left">
            <span class="status-dot ${svc.status}"></span>
            <div>
              <div class="service-name">${svc.service.replace(/_/g, ' ')}</div>
              <div class="status-label">${STATUS_LABEL[svc.status] || svc.status}</div>
            </div>
          </div>
          <div class="right">
            <div class="uptime-bar"><div class="fill" style="width:${svc.uptime_90d}%;background:${color};"></div></div>
            <div class="uptime-pct" style="color:${color};">${svc.uptime_90d}%</div>
          </div>`;
        list.appendChild(row);
      }

      document.getElementById('last-updated').textContent = 'Updated ' + new Date(data.generated_at).toLocaleTimeString();
      document.getElementById('loading').style.display = 'none';
      document.getElementById('content').style.display = 'block';
    } catch (err) {
      document.getElementById('loading').textContent = 'Could not load status. Try refreshing.';
    }
  }

  load();
  setInterval(load, 60000);
})();
