// src/core/proxy — payload hashing, Redis dedup, circuit breaker, and the
// unified request forwarder. Moved out of server.js per RESTRUCTURE_PLAN.md
// Phase 3 — logic unchanged, only how free variables are bound changed
// (closure over server.js module scope -> explicit `deps` passed in here).
//
// createProxy(deps) returns the functions server.js's routes and
// src/core/mcp both call into. Circuit breaker state (getCircuitStatesBatch)
// is also consumed directly by the dashboard's /api/v1/services endpoint.
const { TokenBucket } = require('./token-bucket');

function createProxy(deps) {
  const {
    redis,
    fastify,
    axios,
    SERVICE_ROUTES,
    validateFields,
    getEffectiveValidationRule,
    resolveCustomRoute,
    verifyApiKey,
    getEffectiveRateLimit,
    checkUsageLimit,
    incrementMonthlyUsage,
    getCredential,
    logAudit,
    extractUpstreamErrorMessage,
    AGENT_RATE_LIMIT_PER_MIN,
    ENTERPRISE_MODE,
    maintenanceQueue,
    notifyCircuitOpen,
    pg,
    encryptCredential,
    PROXY_RETRY_MAX_ATTEMPTS = 3,
    PROXY_RETRY_BASE_DELAY_MS = 300,
  } = deps;

  function generateRequestId() { return 'req_' + require('crypto').randomBytes(8).toString('hex'); }

  function hashPayload(apiKey, service, action, payload) {
    return require('crypto').createHash('sha256').update(JSON.stringify({ apiKey, service, action, payload })).digest('hex');
  }

  // Token-bucket rate limit for agent-facing traffic (webhook/SDK/MCP), keyed
  // by API key when present or by org+agent identity when anonymous.
  // Community-tier behavior: reject immediately over the cap (tryConsume).
  // Enterprise-tier (ENTERPRISE_MODE): smooth burst traffic instead of
  // rejecting it — acquire() queues the caller (bounded by maxWaitMs) until
  // a token frees up, rather than failing the request outright. This is
  // the actual "Enterprise upgrade" src/ee/rate_limiter/README.md
  // describes; previously wired in but never actually used (tryConsume was
  // called unconditionally for every tier).
  const agentRateLimitBucket = new TokenBucket(redis);
  async function checkAgentRateLimit(identity, limit = AGENT_RATE_LIMIT_PER_MIN) {
    const bucketKey = `ratelimit:agent:${identity}`;
    if (ENTERPRISE_MODE) {
      const result = await agentRateLimitBucket.acquire(bucketKey, limit, limit / 60, { maxWaitMs: 5000 });
      return result.acquired;
    }
    const result = await agentRateLimitBucket.tryConsume(bucketKey, limit, limit / 60);
    return result.allowed;
  }

  // ─── DEDUP CLAIM/RELEASE (atomic — fixes the race condition) ───
  // Two identical requests can arrive within milliseconds of each other. The old
  // code did redis.get() (check) then redis.setex() (act) as two separate steps,
  // so both requests could pass the check before either had written the result —
  // both would then get forwarded upstream, breaking "exactly once".
  //
  // Fix: claim the dedup key atomically with SET ... NX. Only one caller can win
  // the claim. The loser either gets back a completed result (return it as
  // deduplicated) or finds the winner's request still in flight (return 409 so
  // the caller can retry shortly, rather than silently double-executing).
  const DEDUP_TTL_SECONDS = 86400;

  async function claimDedupSlot(dedupHash) {
    const key = `dedup:${dedupHash}`;
    const claimed = await redis.set(key, JSON.stringify({ pending: true }), 'EX', DEDUP_TTL_SECONDS, 'NX');
    return { key, claimed: claimed === 'OK' };
  }

  async function readDedupSlot(key) {
    const existing = await redis.get(key);
    return existing ? JSON.parse(existing) : null;
  }

  async function completeDedupSlot(key, result) {
    await redis.set(key, JSON.stringify(result), 'EX', DEDUP_TTL_SECONDS);
  }

  async function releaseDedupSlot(key) {
    // Only called on failure, so a later retry isn't stuck behind a dead "pending" marker.
    await redis.del(key);
  }

  // ─── CIRCUIT BREAKER ───
  // Best-effort history of every real state transition, for the dashboard's
  // reliability report (uptime %) — never lets a logging failure affect the
  // circuit breaker's own (Redis) behavior, which stays the source of truth
  // for whether traffic is actually blocked.
  async function logCircuitTransition(service, fromState, toState) {
    if (!pg || fromState === toState) return;
    pg.query(
      `INSERT INTO circuit_breaker_events (service, from_state, to_state) VALUES ($1, $2, $3)`,
      [service, fromState, toState]
    ).catch((err) => fastify.log.warn({ err, service, fromState, toState }, 'Circuit transition log failed'));
  }

  async function getCircuitState(service) {
    const key = `circuit:${service}`;
    const state = await redis.get(key);
    if (!state) return 'closed';
    const data = JSON.parse(state);
    if (data.state === 'open') {
      if (Date.now() - data.openedAt > 30000) {
        await redis.setex(key, 3600, JSON.stringify({ state: 'half-open', failures: 0 }));
        logCircuitTransition(service, 'open', 'half-open');
        return 'half-open';
      }
      return 'open';
    }
    return data.state;
  }

  // Batched version of the above for callers that need every service's state at
  // once (e.g. the dashboard's services list, which fires every 12s) — one
  // MGET round-trip instead of N sequential GETs, and any open->half-open
  // transitions are pipelined together instead of each awaiting individually.
  async function getCircuitStatesBatch(services) {
    if (services.length === 0) return {};
    const keys = services.map((s) => `circuit:${s}`);
    const states = await redis.mget(...keys);
    const results = {};
    const pipeline = redis.pipeline();
    let hasWrites = false;

    for (let i = 0; i < services.length; i++) {
      const svc = services[i];
      const state = states[i];
      if (!state) { results[svc] = 'closed'; continue; }
      const data = JSON.parse(state);
      if (data.state === 'open') {
        if (Date.now() - data.openedAt > 30000) {
          results[svc] = 'half-open';
          pipeline.setex(keys[i], 3600, JSON.stringify({ state: 'half-open', failures: 0 }));
          hasWrites = true;
          logCircuitTransition(svc, 'open', 'half-open');
        } else {
          results[svc] = 'open';
        }
      } else {
        results[svc] = data.state;
      }
    }
    if (hasWrites) await pipeline.exec();
    return results;
  }

  async function recordFailure(service) {
    const key = `circuit:${service}`;
    const state = await redis.get(key);
    let data = state ? JSON.parse(state) : { state: 'closed', failures: 0 };
    const fromState = data.state;
    data.failures = (data.failures || 0) + 1;
    if (data.state === 'half-open') { data.state = 'open'; data.openedAt = Date.now(); }
    else if (data.failures >= 5) { data.state = 'open'; data.openedAt = Date.now(); }
    await redis.setex(key, 3600, JSON.stringify(data));
    logCircuitTransition(service, fromState, data.state);
  }

  // A successful call is the only signal that a half-open probe actually
  // worked — without this, nothing ever closed the circuit again after a
  // trip: getCircuitState's lazy open->half-open transition happens after
  // 30s regardless of real traffic, but no code path ever moved half-open
  // back to closed, so a recovered service stayed reported as half-open
  // forever (harmless for traffic, since half-open isn't blocked — but it
  // meant "closed" never showed up again in circuit state/history after a
  // single trip, undermining any uptime reporting built on it). No-ops (no
  // Redis write at all) when already closed, so the steady-state hot path
  // only pays for one extra GET per successful call.
  async function recordSuccess(service) {
    const key = `circuit:${service}`;
    const state = await redis.get(key);
    if (!state) return;
    const data = JSON.parse(state);
    if (data.state !== 'half-open') return;
    await redis.setex(key, 3600, JSON.stringify({ state: 'closed', failures: 0 }));
    logCircuitTransition(service, 'half-open', 'closed');
  }

  // ─── FORWARDER ───
  async function forwardAction(route, serviceName, actionName, orgId, payload, reqId) {
    const credentialKey = route.credentialKey || serviceName;
    const credential = await getCredential(credentialKey, orgId);

    if (!route.internal && route.authType !== 'none' && !credential) {
      throw new Error(`No credentials configured for ${serviceName}. Add them from the dashboard's Credentials panel.`);
    }

    let headers = { 'Content-Type': route.contentType, 'X-AgentRaaS-ReqId': reqId };
    if (route.extraHeaders) {
      headers = { ...headers, ...route.extraHeaders };
    }

    if (route.authType === 'basic' && credential) {
      const username = credential.username ?? credential.api_key ?? '';
      const password = credential.password ?? '';
      headers['Authorization'] = 'Basic ' + Buffer.from(`${username}:${password}`).toString('base64');
    } else if (route.authType === 'custom-header' && credential && route.authHeader) {
      // Custom actions with a user-named header (not necessarily "Authorization") —
      // sent as-is, no "Bearer " prefix assumed.
      headers[route.authHeader] = credential.api_key ?? credential.username ?? '';
    } else if (route.authHeader && credential) {
      const key = credential.api_key ?? credential.username ?? '';
      headers[route.authHeader] = `${route.authHeader === 'Authorization' ? 'Bearer ' : ''}${key}`;
    }

    const url = route.url.replace(/{(\w+)}/g, (match, key) => process.env[key] || match);

    const response = await axios({
      method: route.method,
      url: url,
      headers,
      data: payload,
      timeout: 30000,
      validateStatus: () => true,
    });

    if (response.status >= 400) {
      const error = new Error(extractUpstreamErrorMessage(response.data) || `HTTP ${response.status}`);
      error.response = response;
      throw error;
    }

    // Slack's Web API always returns HTTP 200, even on failure — the real
    // result is response.data.ok, with the error code in response.data.error
    // (e.g. "channel_not_found", "invalid_auth", "not_in_channel"). Without
    // this check, a failed Slack call would be silently marked successful.
    if (serviceName === 'slack' && response.data && response.data.ok === false) {
      const error = new Error(response.data.error || 'Slack API returned ok:false');
      error.response = response;
      throw error;
    }

    return {
      service: serviceName,
      action: actionName,
      forwarded: true,
      upstream_status: response.status,
      upstream_id: response.data?.id || response.data?.object_id || response.data?.sid || null,
      upstream_response: response.data,
      timestamp: new Date().toISOString(),
    };
  }

  // A 4xx (other than 429) means the request itself is the problem — bad
  // input, bad auth, a card that was actually declined — and retrying an
  // identical payload can't fix that, only waste time and attempts. A
  // missing response (network error, timeout, DNS failure) or a 429/5xx is
  // transient and worth retrying.
  function isRetryableError(err) {
    if (!err.response) return true;
    const status = err.response.status;
    return status === 429 || (status >= 500 && status <= 599);
  }

  function sleep(ms) { return new Promise((resolve) => setTimeout(resolve, ms)); }

  // Wraps forwardAction with retry-with-backoff for transient upstream
  // failures. Circuit-breaker-aware in both directions: every failed
  // attempt records a real failure (recordFailure — same honest signal of
  // upstream health as before retries existed, just now possibly firing
  // more than once per logical request), and a retry is abandoned the
  // moment the breaker itself trips open mid-sequence rather than
  // continuing to hammer a service every other caller is now being
  // shielded from. err.circuitAlreadyRecorded lets the caller's own catch
  // block (handleRequest / handleMCP) know not to record the same failure
  // a second time.
  async function forwardWithRetry(route, serviceName, actionName, orgId, payload, reqId, circuitKey) {
    let lastError;
    for (let attempt = 1; attempt <= PROXY_RETRY_MAX_ATTEMPTS; attempt++) {
      try {
        const result = await forwardAction(route, serviceName, actionName, orgId, payload, reqId);
        if (attempt > 1) result.retried = attempt - 1;
        return result;
      } catch (err) {
        lastError = err;
        err.circuitAlreadyRecorded = true;
        await recordFailure(circuitKey);
        if (attempt === PROXY_RETRY_MAX_ATTEMPTS || !isRetryableError(err)) throw err;
        if ((await getCircuitState(circuitKey)) === 'open') throw err;
        const delay = PROXY_RETRY_BASE_DELAY_MS * 2 ** (attempt - 1) + Math.floor(Math.random() * 100);
        fastify.log.warn({ reqId, service: serviceName, action: actionName, attempt, delay_ms: delay, err: err.message }, 'AgentRaaS: retrying transient upstream failure');
        await sleep(delay);
      }
    }
    throw lastError;
  }

  // Multi-Destination Fan-Out (event broadcasting) — best-effort copies of
  // the same payload to every configured fanout_urls destination, after
  // the primary target_url call has already succeeded. Never awaited by
  // the caller and never affects the primary response, the dedup outcome,
  // or the audit log status: a fan-out destination being down is that
  // destination's problem, not a reason to fail (or retry, and risk
  // double-executing) the action the caller actually asked for.
  async function broadcastFanout(route, payload, reqId) {
    if (!route.fanoutUrls || route.fanoutUrls.length === 0) return;
    await Promise.allSettled(
      route.fanoutUrls.map((url) =>
        axios({
          method: 'POST',
          url,
          headers: { 'Content-Type': 'application/json', 'X-AgentRaaS-ReqId': reqId, 'X-AgentRaaS-Fanout': 'true' },
          data: payload,
          timeout: 10000,
          validateStatus: () => true,
        }).catch((err) => {
          fastify.log.warn({ url, reqId, err: err.message }, 'Fan-out broadcast failed (best-effort, not retried)');
        })
      )
    );
  }

  // ─── UNIFIED HANDLER ───
  async function handleRequest(request, reply, source) {
    const reqId = generateRequestId();
    let orgId, agentId, apiKey, service, action, payload;

    if (source === 'webhook') {
      ({ orgId, agentId } = request.params);
      apiKey = request.headers.authorization?.replace('Bearer ', '') || 'anonymous';
      ({ service, action, payload } = request.body || {});
    } else {
      ({ service, action } = request.params);
      apiKey = request.headers['x-agentraas-key'] || 'anonymous';
      payload = request.body || {};
      // SDK callers can identify themselves via these headers so a Connect-Agent-issued
      // key is actually enforced for them too; falls back to a shared generic identity
      // if omitted (keeps existing untagged SDK usage working, unenforced, as before).
      orgId = request.headers['x-agentraas-org'] || 'sdk';
      agentId = request.headers['x-agentraas-agent'] || 'sdk-agent';
    }

    if (!service || !action) return reply.status(400).send({ error: 'Missing service or action', reqId });
    const routeKey = `${service}.${action}`;

    let resolvedRoute;
    if (service === 'custom') {
      resolvedRoute = await resolveCustomRoute(orgId, action);
      if (!resolvedRoute) return reply.status(400).send({ error: `No custom action named "${action}" registered for this org. Register it from the dashboard's Custom Actions panel.`, reqId });
    } else {
      resolvedRoute = SERVICE_ROUTES[routeKey];
      if (!resolvedRoute) return reply.status(400).send({ error: `Unknown service.action: ${routeKey}`, reqId });
    }

    const verify = await verifyApiKey(apiKey, orgId, agentId);
    if (!verify.ok) {
      return reply.status(401).send({ error: 'Invalid or missing API key for this agent. Generate one from the dashboard\'s Connect Agent panel.', reqId });
    }

    // Pause & Buffer (Enterprise) — while maintenance mode is on, incoming
    // webhooks are queued instead of forwarded, so upstream callers (Stripe,
    // GitHub, etc.) see a clean 202 instead of failures during a known
    // maintenance window or downstream outage. Checked after API-key
    // verification (above) so an unauthenticated/garbage request can't fill
    // the queue. SDK/MCP traffic isn't buffered — an agent calling a tool
    // is waiting synchronously for a result, there's no clean way to defer
    // that — this only applies to the fire-and-forget webhook path.
    if (source === 'webhook' && ENTERPRISE_MODE && await maintenanceQueue.isPaused()) {
      await maintenanceQueue.enqueue({ orgId, agentId, apiKey, body: { service, action, payload } });
      return reply.status(202).send({
        buffered: true,
        reqId,
        message: 'AgentRaaS is in maintenance mode — this request has been queued and will be processed automatically once maintenance ends.',
      });
    }

    const rateLimitIdentity = apiKey !== 'anonymous' ? apiKey : `${orgId}:${agentId}`;
    const withinLimit = await checkAgentRateLimit(rateLimitIdentity, await getEffectiveRateLimit(orgId));
    if (!withinLimit) {
      return reply.status(429).send({ error: 'Rate limit exceeded for this agent. Slow down and try again shortly.', reqId });
    }

    const startTime = Date.now();
    let status = 'success', errorType = null;
    const dedupHash = hashPayload(apiKey, service, action, payload);
    const { key: dedupKey, claimed } = await claimDedupSlot(dedupHash);

    if (!claimed) {
      const existing = await readDedupSlot(dedupKey);
      if (!existing || existing.pending) {
        // Another identical request is currently in flight — don't double-execute.
        status = 'blocked'; errorType = 'duplicate_in_progress';
        await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash);
        return reply.status(409).send({ error: 'An identical request is already being processed. Retry shortly.', reqId });
      }
      status = 'deduplicated';
      await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash);
      return reply.status(200).send({ ...existing, cached: true, reqId });
    }

    try {
      // Curated services validate against config/services.json's static rules
      // by default; an org can override or add to that from the dashboard's
      // Validation Rules panel (custom_validation_rules table) — that always
      // wins when one exists. Custom Actions have no static fallback at all,
      // so a custom rule is the only way to get validation on one.
      const effectiveRule = await getEffectiveValidationRule(orgId, service, action);
      if (effectiveRule) {
        const validationError = validateFields(payload, effectiveRule.fields);
        if (validationError) {
          status = 'blocked'; errorType = 'validation_failed';
          await releaseDedupSlot(dedupKey);
          await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash);
          return reply.status(422).send({ error: validationError, reqId });
        }
      }

      const circuitKey = resolvedRoute.credentialKey || service;
      const circuitState = await getCircuitState(circuitKey);
      if (circuitState === 'open') {
        status = 'blocked'; errorType = 'circuit_open';
        await releaseDedupSlot(dedupKey);
        await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash);
        notifyCircuitOpen(orgId, service).catch(() => {}); // fire-and-forget, rate-limited internally
        return reply.status(503).send({ error: `Circuit breaker open for ${service}. Try again later.`, reqId });
      }

      const usageCheck = await checkUsageLimit(orgId);
      if (!usageCheck.ok) {
        status = 'blocked'; errorType = 'usage_limit_exceeded';
        await releaseDedupSlot(dedupKey);
        await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash);
        return reply.status(402).send({
          error: `Monthly usage limit reached (${usageCheck.count}/${usageCheck.limit} actions this month). Contact support@agentraas.io to upgrade.`,
          reqId,
        });
      }

      const result = await forwardWithRetry(resolvedRoute, service, action, orgId, payload, reqId, circuitKey);
      recordSuccess(circuitKey).catch(() => {}); // fire-and-forget — closes a half-open circuit on recovery, never blocks the response
      broadcastFanout(resolvedRoute, payload, reqId).catch(() => {}); // fire-and-forget, see broadcastFanout's own error handling
      await completeDedupSlot(dedupKey, result);
      await incrementMonthlyUsage(orgId);
      await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash, payload);
      return reply.status(200).send({ ...result, reqId });
    } catch (err) {
      status = 'error';
      const upstreamMessage = extractUpstreamErrorMessage(err.response?.data);
      errorType = upstreamMessage || err.message; // full detail still kept in the account's own audit log
      await releaseDedupSlot(dedupKey);
      if (!err.circuitAlreadyRecorded) await recordFailure(resolvedRoute.credentialKey || service);
      await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, null);
      fastify.log.error({ err, reqId }, 'Request failed');
      // Upstream provider errors (Stripe, Twilio, etc.) are useful for the caller to debug
      // their own integration — pass those through. If AgentRaaS itself failed (no upstream
      // response at all — a DB/network/code error), don't leak internals like hostnames or
      // stack fragments to an external caller; return a generic message instead.
      const responseMessage = err.response
        ? (upstreamMessage || 'Upstream service returned an error.')
        : 'An internal error occurred while processing this request.';
      // Dead Letter Queue — only for genuine upstream failures (err.response
      // present: the target API itself returned an error), not client-side
      // rejections (validation, usage limit, circuit already open) that a
      // blind replay wouldn't fix. Best-effort: never let a DLQ write
      // failure change the response the caller already gets.
      if (err.response && pg && encryptCredential) {
        pg.query(
          `INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message)
           VALUES ($1, $2, $3, $4, $5, $6, $7)`,
          [reqId, orgId, agentId, service, action, encryptCredential(JSON.stringify(payload)), errorType]
        ).catch((dlqErr) => fastify.log.warn({ dlqErr, reqId }, 'Dead-letter queue write failed'));
      }
      return reply.status(err.response?.status || 500).send({ error: responseMessage, reqId, agentraas_note: 'Request blocked by AgentRaaS.' });
    }
  }

  // Replays every currently-buffered maintenance-mode webhook through the
  // exact same handleRequest() a live request goes through — a synthetic
  // request/reply pair rather than a second copy of the pipeline logic, so
  // dedup/validation/circuit-breaker/forward/audit behave identically to a
  // real request (including exactly-once — a buffered request that somehow
  // already got processed via its dedup hash is just a no-op replay here).
  async function flushMaintenanceQueue() {
    return maintenanceQueue.drain(async (item) => {
      const fakeRequest = {
        params: { orgId: item.orgId, agentId: item.agentId },
        headers: { authorization: `Bearer ${item.apiKey}` },
        body: item.body,
      };
      const outcome = { status: 200, body: null };
      const fakeReply = {
        status(code) { outcome.status = code; return fakeReply; },
        send(body) { outcome.body = body; return outcome; },
      };
      await handleRequest(fakeRequest, fakeReply, 'webhook');
      // 409 (an identical request already in flight) is a benign race, not
      // a real failure — everything else 4xx/5xx counts as a failed replay.
      if (outcome.status >= 400 && outcome.status !== 409) {
        throw new Error(`Buffered request replay failed with status ${outcome.status}: ${JSON.stringify(outcome.body)}`);
      }
    });
  }

  return {
    generateRequestId,
    hashPayload,
    checkAgentRateLimit,
    claimDedupSlot,
    readDedupSlot,
    completeDedupSlot,
    releaseDedupSlot,
    getCircuitState,
    getCircuitStatesBatch,
    recordFailure,
    recordSuccess,
    forwardAction,
    forwardWithRetry,
    broadcastFanout,
    handleRequest,
    flushMaintenanceQueue,
  };
}

module.exports = { createProxy };
