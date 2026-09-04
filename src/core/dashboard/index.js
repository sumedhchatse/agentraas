// src/core/dashboard — the dashboard's own read/stats/admin API endpoints.
// Moved out of server.js per RESTRUCTURE_PLAN.md Phase 3 — logic
// unchanged, route bodies moved verbatim; only how free variables are
// bound changed (closure over server.js module scope -> explicit `deps`).
// The static frontend (public/) is not part of this move.
function registerDashboardRoutes(fastify, deps) {
  const {
    pg,
    redis,
    getUserOrgIds,
    getMonthlyUsage,
    getEffectiveLimit,
    currentMonthKey,
    getCircuitStatesBatch,
    DASHBOARD_RANGES,
    DASHBOARD_BUCKETS,
    DEPLOYMENT_MODE,
    SERVICE_CONFIG,
    requireAuthRateLimited,
    requireAdminRateLimited,
    usageEvents,
  } = deps;

  fastify.get('/api/v1/stats', { preHandler: requireAuthRateLimited }, async (request) => {
    const orgIds = await getUserOrgIds(request.user.sub);
    if (orgIds.length === 0) return { total: 0, success: 0, deduplicated: 0, blocked: 0, errors: 0, avg_duration: null };
    const today = new Date().toISOString().split('T')[0];
    const r = await pg.query(`SELECT COUNT(*) as total, COUNT(*) FILTER(WHERE status='success') as success, COUNT(*) FILTER(WHERE status='deduplicated') as deduplicated, COUNT(*) FILTER(WHERE status='blocked') as blocked, COUNT(*) FILTER(WHERE status='error') as errors, AVG(duration_ms) as avg_duration FROM audit_log WHERE created_at >= $1 AND org_id = ANY($2)`, [today, orgIds]);
    return r.rows[0];
  });

  // Ranged stats for the dashboard's 24h / 7d / 30d / 90d tabs.
  fastify.get('/api/v1/dashboard/stats', { preHandler: requireAuthRateLimited }, async (request, reply) => {
    const range = request.query.range || '24h';
    const interval = DASHBOARD_RANGES[range];
    if (!interval) {
      return reply.status(400).send({ error: 'Invalid range. Use one of: 24h, 7d, 30d, 90d.' });
    }
    const orgIds = await getUserOrgIds(request.user.sub);
    if (orgIds.length === 0) return { range, total: 0, success: 0, deduplicated: 0, blocked: 0, errors: 0, avg_duration: null };
    // interval is looked up from the fixed DASHBOARD_RANGES map above, never taken
    // directly from the request, so this interpolation can't carry user input.
    const r = await pg.query(
      `SELECT COUNT(*) as total,
              COUNT(*) FILTER(WHERE status='success') as success,
              COUNT(*) FILTER(WHERE status='deduplicated') as deduplicated,
              COUNT(*) FILTER(WHERE status='blocked') as blocked,
              COUNT(*) FILTER(WHERE status='error') as errors,
              AVG(duration_ms) as avg_duration
       FROM audit_log
       WHERE created_at >= NOW() - INTERVAL '${interval}' AND org_id = ANY($1)`,
      [orgIds]
    );
    return { range, ...r.rows[0] };
  });

  // Bucketed counts for a simple activity chart, same range options as above.
  fastify.get('/api/v1/dashboard/timeseries', { preHandler: requireAuthRateLimited }, async (request, reply) => {
    const range = request.query.range || '24h';
    const interval = DASHBOARD_RANGES[range];
    const bucket = DASHBOARD_BUCKETS[range];
    if (!interval) {
      return reply.status(400).send({ error: 'Invalid range. Use one of: 24h, 7d, 30d, 90d.' });
    }
    const orgIds = await getUserOrgIds(request.user.sub);
    if (orgIds.length === 0) return { range, bucket, points: [] };
    const r = await pg.query(
      `SELECT date_trunc('${bucket}', created_at) as bucket, COUNT(*) as total
       FROM audit_log
       WHERE created_at >= NOW() - INTERVAL '${interval}' AND org_id = ANY($1)
       GROUP BY bucket
       ORDER BY bucket ASC`,
      [orgIds]
    );
    return { range, bucket, points: r.rows };
  });

  // Per-service request counts for the selected range, used by the dashboard's
  // service breakdown chart. Capped to the top 8 services so the chart stays legible.
  fastify.get('/api/v1/dashboard/by-service', { preHandler: requireAuthRateLimited }, async (request, reply) => {
    const range = request.query.range || '24h';
    const interval = DASHBOARD_RANGES[range];
    if (!interval) {
      return reply.status(400).send({ error: 'Invalid range. Use one of: 24h, 7d, 30d, 90d.' });
    }
    const orgIds = await getUserOrgIds(request.user.sub);
    if (orgIds.length === 0) return { range, services: [] };
    const r = await pg.query(
      `SELECT service, COUNT(*) as total
       FROM audit_log
       WHERE created_at >= NOW() - INTERVAL '${interval}' AND org_id = ANY($1)
       GROUP BY service
       ORDER BY total DESC
       LIMIT 8`,
      [orgIds]
    );
    return { range, services: r.rows };
  });

  fastify.get('/api/v1/recent', { preHandler: requireAuthRateLimited }, async (request) => {
    const limit = parseInt(request.query.limit) || 50;
    const orgIds = await getUserOrgIds(request.user.sub);
    if (orgIds.length === 0) return [];
    const r = await pg.query(`SELECT req_id, api_key, org_id, agent_id, service, action, status, error_type, duration_ms, created_at FROM audit_log WHERE org_id = ANY($1) ORDER BY created_at DESC LIMIT $2`, [orgIds, limit]);
    return r.rows;
  });

  fastify.get('/api/v1/agents', { preHandler: requireAuthRateLimited }, async (request) => {
    const orgIds = await getUserOrgIds(request.user.sub);
    if (orgIds.length === 0) return [];
    const r = await pg.query(`SELECT org_id, agent_id, COUNT(*) as total_actions, MAX(created_at) as last_seen FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours' AND org_id = ANY($1) GROUP BY org_id, agent_id ORDER BY total_actions DESC`, [orgIds]);
    return r.rows;
  });

  fastify.get('/api/v1/usage', { preHandler: requireAuthRateLimited }, async (request) => {
    const adminCheck = await pg.query('SELECT is_admin FROM users WHERE id = $1', [request.user.sub]);
    const isAdmin = adminCheck.rows.length > 0 && adminCheck.rows[0].is_admin;
    // Local-range accounts (id 1-9 — personal/founder accounts) are exempt
    // from enforcement too, matching checkUsageLimit's broader exemption rule.
    const isExempt = isAdmin || (request.user.sub >= 1 && request.user.sub <= 9);

    // A user can own multiple orgs (via their default org, api_keys, custom
    // actions, or credentials they've set up) — aggregate this month's usage
    // across all of them.
    const orgIds = await getUserOrgIds(request.user.sub);

    const perOrg = [];
    let total = 0;
    // Self-hosted is always unlimited (see LICENSE.md Section 2 and
    // checkUsageLimit) — no per-org limit math applies there, so `limit`
    // stays null (meaning "unlimited") rather than summing a fake cap.
    let limit = DEPLOYMENT_MODE === 'cloud' ? 0 : null;
    for (const orgId of orgIds) {
      const count = await getMonthlyUsage(orgId);
      total += count;
      if (DEPLOYMENT_MODE === 'cloud') {
        const orgLimit = await getEffectiveLimit(orgId);
        perOrg.push({ org_id: orgId, count, limit: orgLimit });
        limit += orgLimit;
      } else {
        perOrg.push({ org_id: orgId, count, limit: null });
      }
    }

    return {
      deployment_mode: DEPLOYMENT_MODE,
      limit,
      unlimited: limit === null,
      // Exempt orgs (admin, or local-range accounts) aren't enforced (see
      // checkUsageLimit) — reflect that here too, so the dashboard doesn't
      // show a cap that doesn't actually apply.
      enforced: DEPLOYMENT_MODE === 'cloud' && !isExempt,
      exempt: isExempt,
      total,
      per_org: perOrg,
    };
  });

  // Server-Sent Events stream of usage updates for the logged-in user's
  // orgs — pushed the instant an action increments usage (see
  // incrementMonthlyUsage's redis.publish in server.js), rather than
  // waiting for the dashboard's next poll interval. The client (public/
  // index.html) uses this to keep the header call-count live; polling via
  // GET /api/v1/usage above still works as a fallback if SSE is unavailable.
  fastify.get('/api/v1/usage/stream', { preHandler: requireAuthRateLimited }, async (request, reply) => {
    const orgIds = new Set(await getUserOrgIds(request.user.sub));

    reply.raw.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache',
      Connection: 'keep-alive',
    });
    reply.raw.write(':ok\n\n'); // opens the stream immediately so the client's onopen fires

    const onUpdate = (event) => {
      if (!orgIds.has(event.org_id)) return;
      reply.raw.write(`data: ${JSON.stringify(event)}\n\n`);
    };
    usageEvents.on('update', onUpdate);

    // Keeps intermediary proxies from timing out an idle connection.
    const keepalive = setInterval(() => reply.raw.write(':keepalive\n\n'), 20000);

    request.raw.on('close', () => {
      clearInterval(keepalive);
      usageEvents.off('update', onUpdate);
    });

    // Fastify expects the handler to return/resolve, but this connection
    // stays open indefinitely (closed by the client or request.raw's
    // 'close' event above) — hang here without ever resolving.
    return new Promise(() => {});
  });

  fastify.get('/api/v1/admin/users', { preHandler: requireAdminRateLimited }, async () => {
    const usersResult = await pg.query(
      `SELECT id, email, is_admin, email_verified, created_at, last_login_at, plan FROM users ORDER BY id ASC`
    );
    const users = usersResult.rows;
    if (users.length === 0) return [];

    // Every user's org associations and per-table counts, in one query each —
    // not one set of queries per user (was up to 5*N round-trips for N users,
    // now a fixed 4 Postgres queries + 1 Redis MGET regardless of user count).
    const [orgRows, customActionsRows, credentialsRows, apiKeysRows] = await Promise.all([
      pg.query(
        `SELECT user_id, org_id FROM api_keys
         UNION SELECT user_id, org_id FROM custom_actions
         UNION SELECT user_id, org_id FROM service_credentials`
      ),
      pg.query('SELECT user_id, COUNT(*) as count FROM custom_actions GROUP BY user_id'),
      pg.query('SELECT user_id, COUNT(*) as count FROM service_credentials WHERE revoked_at IS NULL GROUP BY user_id'),
      pg.query('SELECT user_id, COUNT(*) as count FROM api_keys GROUP BY user_id'),
    ]);

    const orgsByUser = {};
    const allOrgIds = new Set();
    for (const row of orgRows.rows) {
      if (!orgsByUser[row.user_id]) orgsByUser[row.user_id] = new Set();
      orgsByUser[row.user_id].add(row.org_id);
      allOrgIds.add(row.org_id);
    }
    const customActionsByUser = Object.fromEntries(customActionsRows.rows.map((r) => [r.user_id, parseInt(r.count, 10)]));
    const credentialsByUser = Object.fromEntries(credentialsRows.rows.map((r) => [r.user_id, parseInt(r.count, 10)]));
    const apiKeysByUser = Object.fromEntries(apiKeysRows.rows.map((r) => [r.user_id, parseInt(r.count, 10)]));

    // Usage-this-month is Redis-backed (see getMonthlyUsage) — one MGET for
    // every distinct org across all users, instead of one GET per org per user.
    const orgIdList = [...allOrgIds];
    const usageByOrg = {};
    if (orgIdList.length > 0) {
      const keys = orgIdList.map((orgId) => `usage:${orgId}:${currentMonthKey()}`);
      const values = await redis.mget(...keys);
      orgIdList.forEach((orgId, i) => { usageByOrg[orgId] = parseInt(values[i] || '0', 10); });
    }

    return users.map((user) => {
      const orgs = orgsByUser[user.id] ? [...orgsByUser[user.id]] : [];
      const usageThisMonth = orgs.reduce((sum, orgId) => sum + (usageByOrg[orgId] || 0), 0);
      return {
        id: user.id,
        email: user.email,
        is_admin: user.is_admin,
        email_verified: user.email_verified,
        created_at: user.created_at,
        last_login_at: user.last_login_at,
        plan: user.plan,
        org_count: orgs.length,
        orgs,
        usage_this_month: usageThisMonth,
        custom_actions_count: customActionsByUser[user.id] || 0,
        credentials_count: credentialsByUser[user.id] || 0,
        api_keys_count: apiKeysByUser[user.id] || 0,
      };
    });
  });

  // NOTE: there is deliberately no admin promote/demote endpoint. Only one
  // admin can exist at all — enforced at the database level with a partial
  // unique index on is_admin=true (see migration 013) — and the only way to
  // create it is infra/scripts/bootstrap-admin.sh, run directly on the server.

  // Unauthenticated, deliberately coarse — global (all-org) 24h counts and
  // average latency only, no per-org/user breakdown or any identifying
  // fields. Powers the landing page's live "execution ledger" panel, which
  // is public by definition (it's shown before login).
  fastify.get('/api/v1/public/execution-ledger', async () => {
    const r = await pg.query(
      `SELECT COUNT(*) as total,
              COUNT(*) FILTER (WHERE status = 'deduplicated') as deduplicated,
              AVG(duration_ms) as avg_duration
       FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours'`
    );
    const row = r.rows[0];
    return {
      actions_verified: parseInt(row.total, 10) || 0,
      duplicates_caught: parseInt(row.deduplicated, 10) || 0,
      avg_duration_ms: row.avg_duration ? Math.round(row.avg_duration) : null,
    };
  });

  fastify.get('/api/v1/admin/overview', { preHandler: requireAdminRateLimited }, async () => {
    const userCount = await pg.query('SELECT COUNT(*) as count FROM users');
    const orgCount = await pg.query(
      `SELECT COUNT(DISTINCT org_id) as count FROM (
         SELECT org_id FROM api_keys UNION SELECT org_id FROM custom_actions UNION SELECT org_id FROM service_credentials
       ) t`
    );
    const monthTotals = await pg.query(
      `SELECT COUNT(*) FILTER (WHERE status='success') as success,
              COUNT(*) FILTER (WHERE status='deduplicated') as deduplicated,
              COUNT(*) FILTER (WHERE status='blocked') as blocked,
              COUNT(*) FILTER (WHERE status='error') as errors
       FROM audit_log WHERE created_at >= date_trunc('month', NOW())`
    );
    return {
      deployment_mode: DEPLOYMENT_MODE,
      total_users: parseInt(userCount.rows[0].count, 10),
      total_orgs: parseInt(orgCount.rows[0].count, 10),
      this_month: monthTotals.rows[0],
    };
  });

  fastify.get('/api/v1/services', { preHandler: requireAuthRateLimited }, async (request) => {
    const services = Object.keys(SERVICE_CONFIG);
    const orgIds = await getUserOrgIds(request.user.sub);
    const [circuitStates, statsResult] = await Promise.all([
      getCircuitStatesBatch(services),
      orgIds.length === 0
        ? Promise.resolve({ rows: [] })
        : pg.query(
            `SELECT service, COUNT(*) as total, MAX(created_at) as last_used
             FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours' AND service = ANY($1) AND org_id = ANY($2)
             GROUP BY service`,
            [services, orgIds]
          ),
    ]);
    const statsMap = {};
    for (const row of statsResult.rows) {
      statsMap[row.service] = { total: parseInt(row.total, 10) || 0, last_used: row.last_used };
    }
    return services.map((svc) => ({
      name: svc,
      circuit_state: circuitStates[svc],
      actions_24h: statsMap[svc]?.total || 0,
      last_used: statsMap[svc]?.last_used || null,
    }));
  });

  // ─── RELIABILITY REPORT ─── turns the circuit-breaker history (see
  // circuit_breaker_events, migration 030) and audit_log into the kind of
  // number a customer actually wants to see: uptime %, success rate, and
  // duplicates prevented, per service, over the same 24h/7d/30d/90d ranges
  // used elsewhere on the dashboard.
  //
  // Uptime is reconstructed from consecutive state-transition events with a
  // LEAD window (pair each "...->open" event with whatever transition comes
  // next, or NOW() if it's still open) rather than tracked as a running
  // total, so it stays correct no matter how the report's own range changes
  // from call to call. Circuit state is shared across every org calling a
  // service (one circuit per service, not per org — same as the Redis
  // circuit:<service> keys it mirrors), so uptime is a platform-wide number;
  // success rate and duplicates prevented stay scoped to the caller's own
  // orgs, same as every other stat on this dashboard.
  fastify.get('/api/v1/reliability-report', { preHandler: requireAuthRateLimited }, async (request, reply) => {
    const range = request.query.range || '24h';
    const interval = DASHBOARD_RANGES[range];
    if (!interval) {
      return reply.status(400).send({ error: 'Invalid range. Use one of: 24h, 7d, 30d, 90d.' });
    }
    const services = Object.keys(SERVICE_CONFIG);
    const orgIds = await getUserOrgIds(request.user.sub);

    const [uptimeResult, statsResult] = await Promise.all([
      pg.query(
        `WITH events AS (
           SELECT service, to_state, occurred_at,
                  LEAD(occurred_at) OVER (PARTITION BY service ORDER BY occurred_at) AS next_at
           FROM circuit_breaker_events
           WHERE service = ANY($1) AND occurred_at >= NOW() - INTERVAL '${interval}'
         )
         SELECT service,
                COALESCE(SUM(EXTRACT(EPOCH FROM (LEAST(COALESCE(next_at, NOW()), NOW()) - occurred_at))) FILTER (WHERE to_state = 'open'), 0) AS open_seconds
         FROM events
         GROUP BY service`,
        [services]
      ),
      orgIds.length === 0
        ? Promise.resolve({ rows: [] })
        : pg.query(
            `SELECT service,
                    COUNT(*) FILTER (WHERE status = 'success') AS success,
                    COUNT(*) FILTER (WHERE status = 'error') AS errors,
                    COUNT(*) FILTER (WHERE status = 'deduplicated') AS duplicates_prevented,
                    COUNT(*) AS total_actions
             FROM audit_log
             WHERE created_at >= NOW() - INTERVAL '${interval}' AND service = ANY($1) AND org_id = ANY($2)
             GROUP BY service`,
            [services, orgIds]
          ),
    ]);

    const openSecondsMap = {};
    for (const row of uptimeResult.rows) openSecondsMap[row.service] = parseFloat(row.open_seconds) || 0;
    const statsMap = {};
    for (const row of statsResult.rows) statsMap[row.service] = row;

    // Same interval string DASHBOARD_RANGES already validated above, so this
    // is a fixed set of literals, never request input.
    const RANGE_SECONDS = { '24h': 86400, '7d': 604800, '30d': 2592000, '90d': 7776000 };
    const rangeSeconds = RANGE_SECONDS[range];

    const report = services.map((svc) => {
      const openSeconds = Math.min(openSecondsMap[svc] || 0, rangeSeconds);
      const uptimePct = Math.round((1 - openSeconds / rangeSeconds) * 10000) / 100;
      const stats = statsMap[svc];
      const success = stats ? parseInt(stats.success, 10) : 0;
      const errors = stats ? parseInt(stats.errors, 10) : 0;
      const successRate = success + errors > 0 ? Math.round((success / (success + errors)) * 10000) / 100 : null;
      return {
        service: svc,
        uptime_pct: uptimePct,
        success_rate: successRate,
        duplicates_prevented: stats ? parseInt(stats.duplicates_prevented, 10) : 0,
        total_actions: stats ? parseInt(stats.total_actions, 10) : 0,
      };
    });

    return { range, services: report };
  });

  fastify.get('/api/v1/export/csv', { preHandler: requireAuthRateLimited }, async (request, reply) => {
    const orgIds = await getUserOrgIds(request.user.sub);
    const headers = ['Timestamp', 'Request ID', 'Org', 'Agent', 'Service', 'Action', 'Status', 'Error', 'Duration_ms'];
    if (orgIds.length === 0) {
      reply.header('Content-Type', 'text/csv').header('Content-Disposition', 'attachment; filename="agentraas-audit.csv"').send(headers.join(','));
      return;
    }
    const r = await pg.query(`SELECT created_at, req_id, org_id, agent_id, service, action, status, error_type, duration_ms FROM audit_log WHERE org_id = ANY($1) ORDER BY created_at DESC LIMIT 10000`, [orgIds]);
    const rows = r.rows.map((row) => [row.created_at, row.req_id, row.org_id, row.agent_id, row.service, row.action, row.status, row.error_type || '', row.duration_ms].map((f) => `"${String(f).replace(/"/g, '""')}"`).join(','));
    reply.header('Content-Type', 'text/csv').header('Content-Disposition', 'attachment; filename="agentraas-audit.csv"').send([headers.join(','), ...rows].join('\n'));
  });
}

module.exports = { registerDashboardRoutes };
