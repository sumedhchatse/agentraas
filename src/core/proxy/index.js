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
    VALIDATION_RULES,
    validatePayload,
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
  async function getCircuitState(service) {
    const key = `circuit:${service}`;
    const state = await redis.get(key);
    if (!state) return 'closed';
    const data = JSON.parse(state);
    if (data.state === 'open') {
      if (Date.now() - data.openedAt > 30000) {
        await redis.setex(key, 3600, JSON.stringify({ state: 'half-open', failures: 0 }));
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
    data.failures = (data.failures || 0) + 1;
    if (data.state === 'half-open') { data.state = 'open'; data.openedAt = Date.now(); }
    else if (data.failures >= 5) { data.state = 'open'; data.openedAt = Date.now(); }
    await redis.setex(key, 3600, JSON.stringify(data));
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
      // Custom actions have no pre-defined validation rules (the human who registered
      // it owns responsibility for what payload shape it expects) — only curated
      // services go through the config-driven validator.
      if (service !== 'custom') {
        const validationError = validatePayload(service, action, payload, VALIDATION_RULES);
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

      const result = await forwardAction(resolvedRoute, service, action, orgId, payload, reqId);
      await completeDedupSlot(dedupKey, result);
      await incrementMonthlyUsage(orgId);
      await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, dedupHash, payload);
      return reply.status(200).send({ ...result, reqId });
    } catch (err) {
      status = 'error';
      const upstreamMessage = extractUpstreamErrorMessage(err.response?.data);
      errorType = upstreamMessage || err.message; // full detail still kept in the account's own audit log
      await releaseDedupSlot(dedupKey);
      await recordFailure(resolvedRoute.credentialKey || service);
      await logAudit(reqId, apiKey, orgId, agentId, service, action, status, errorType, Date.now() - startTime, null);
      fastify.log.error({ err, reqId }, 'Request failed');
      // Upstream provider errors (Stripe, Twilio, etc.) are useful for the caller to debug
      // their own integration — pass those through. If AgentRaaS itself failed (no upstream
      // response at all — a DB/network/code error), don't leak internals like hostnames or
      // stack fragments to an external caller; return a generic message instead.
      const responseMessage = err.response
        ? (upstreamMessage || 'Upstream service returned an error.')
        : 'An internal error occurred while processing this request.';
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
    forwardAction,
    handleRequest,
    flushMaintenanceQueue,
  };
}

module.exports = { createProxy };
