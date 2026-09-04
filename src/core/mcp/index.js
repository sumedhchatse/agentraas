// src/core/mcp — MCP JSON-RPC handling. Moved out of server.js per
// RESTRUCTURE_PLAN.md Phase 3 — logic unchanged. Reuses the same dedup/
// circuit-breaker/forwarder functions as src/core/proxy (passed in via
// deps.proxy) rather than duplicating them, since MCP is just another
// entry point into the same reliability layer as the webhook/SDK routes.
function createMcp(deps) {
  const {
    fastify,
    proxy,
    SERVICE_CONFIG,
    SERVICE_ROUTES,
    validateFields,
    getEffectiveValidationRule,
    resolveCustomRoute,
    verifyApiKey,
    getEffectiveRateLimit,
    checkUsageLimit,
    incrementMonthlyUsage,
    logAudit,
    extractUpstreamErrorMessage,
    notifyCircuitOpen,
    pg,
    encryptCredential,
  } = deps;
  const {
    generateRequestId,
    hashPayload,
    hashIdempotencyKey,
    hashOnly,
    checkAgentRateLimit,
    claimDedupSlot,
    readDedupSlot,
    completeDedupSlot,
    releaseDedupSlot,
    getCircuitState,
    recordFailure,
    recordSuccess,
    forwardWithRetry,
    broadcastFanout,
  } = proxy;

  // MCP tool names are `${service}_${action.replace(/\./g,'_')}` (e.g.
  // "mockpay_payment_create" for service "mockpay", action "payment.create")
  // — but every configured action follows a dotted resource.verb pattern
  // (payment.create, charge.create, ...) and several service names contain
  // underscores of their own (e.g. "opn_payments"), so there's no way to
  // losslessly reverse a tool name back into (service, action) by splitting
  // on '_' at request time — trying to (as this used to) silently 404s every
  // built-in tool. Build the reverse mapping once, from the same
  // SERVICE_CONFIG the forward name is generated from below, so lookup at
  // call time is exact instead of guessed.
  const TOOL_NAME_TO_ROUTE = {};
  for (const [svcName, svc] of Object.entries(SERVICE_CONFIG)) {
    for (const actName of Object.keys(svc.actions)) {
      TOOL_NAME_TO_ROUTE[`${svcName}_${actName.replace(/\./g, '_')}`] = { serviceName: svcName, actionName: actName };
    }
  }

  async function handleMCP(request, reply) {
    const { jsonrpc, method, params, id } = request.body || {};
    if (jsonrpc !== '2.0') return reply.status(400).send({ jsonrpc: '2.0', error: { code: -32600, message: 'Invalid Request' }, id: id || null });

    if (method === 'tools/list') {
      const tools = Object.entries(SERVICE_CONFIG).flatMap(([svcName, svc]) =>
        Object.entries(svc.actions).map(([actName, act]) => ({
          name: `${svcName}_${actName.replace(/\./g, '_')}`,
          description: `AgentRaaS-protected ${svcName} ${actName}`,
          inputSchema: {
            type: 'object',
            properties: {
              payload: { type: 'object', description: 'Request payload' },
              org_id: { type: 'string', description: 'Organization ID' },
              idempotency_key: { type: 'string', description: 'Optional — dedupe on this key instead of the exact payload bytes, so you control what counts as a retry of the same operation. Reusing the key with a genuinely different payload is rejected (not silently applied), matching Stripe-style idempotency keys.' },
            },
            required: ['payload'],
          },
        }))
      );
      return reply.send({ jsonrpc: '2.0', id, result: { tools } });
    }

    if (method === 'tools/call') {
      const toolName = params?.name;
      const payload = params?.arguments?.payload || {};
      const orgId = params?.arguments?.org_id || 'mcp';
      const agentId = params?.arguments?.agent_id || 'mcp-agent';
      const idempotencyKey = params?.arguments?.idempotency_key || null;
      const apiKey = request.headers['x-agentraas-key'] || 'anonymous';
      const reqId = generateRequestId();

      if (!toolName || typeof toolName !== 'string') {
        return reply.send({ jsonrpc: '2.0', id, error: { code: -32602, message: 'Invalid params: "name" is required and must be a string.' } });
      }

      let resolvedRoute, resolvedServiceName, resolvedActionName;
      const mapped = TOOL_NAME_TO_ROUTE[toolName];
      if (mapped) {
        resolvedServiceName = mapped.serviceName;
        resolvedActionName = mapped.actionName;
        resolvedRoute = SERVICE_ROUTES[`${resolvedServiceName}.${resolvedActionName}`];
      }
      if (!resolvedRoute) {
        // Fall back: treat the whole tool name as a registered custom action name.
        resolvedRoute = await resolveCustomRoute(orgId, toolName);
        if (resolvedRoute) { resolvedServiceName = 'custom'; resolvedActionName = toolName; }
      }
      if (!resolvedRoute) {
        return reply.send({ jsonrpc: '2.0', id, error: { code: -32601, message: `Tool not found: ${toolName}` } });
      }

      const verify = await verifyApiKey(apiKey, orgId, agentId);
      if (!verify.ok) {
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: 'Invalid or missing API key for this agent.', reqId }) }], isError: true } });
      }
      const rateLimitIdentity = apiKey !== 'anonymous' ? apiKey : `${orgId}:${agentId}`;
      const withinLimit = await checkAgentRateLimit(rateLimitIdentity, await getEffectiveRateLimit(orgId));
      if (!withinLimit) {
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: 'Rate limit exceeded for this agent.', reqId }) }], isError: true } });
      }

      const startTime = Date.now();
      const payloadDigest = hashOnly(payload);
      const dedupHash = idempotencyKey ? hashIdempotencyKey(apiKey, resolvedServiceName, resolvedActionName, idempotencyKey) : hashPayload(apiKey, resolvedServiceName, resolvedActionName, payload);
      const { key: dedupKey, claimed } = await claimDedupSlot(dedupHash);

      if (!claimed) {
        const existing = await readDedupSlot(dedupKey);
        if (!existing || existing.pending) {
          await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'duplicate_in_progress', Date.now() - startTime, dedupHash);
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: 'An identical request is already being processed. Retry shortly.', reqId }) }], isError: true } });
        }
        if (idempotencyKey && existing.__payloadDigest && existing.__payloadDigest !== payloadDigest) {
          await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'idempotency_key_reused', Date.now() - startTime, dedupHash);
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: 'This idempotency_key was already used with a different payload. Use a new key for a different request.', reqId }) }], isError: true } });
        }
        await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'deduplicated', null, Date.now() - startTime, dedupHash);
        const { __payloadDigest, ...cached } = existing;
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ ...cached, cached: true, reqId }) }], isError: false } });
      }

      try {
        const effectiveRule = await getEffectiveValidationRule(orgId, resolvedServiceName, resolvedActionName);
        if (effectiveRule) {
          const validationError = validateFields(payload, effectiveRule.fields);
          if (validationError) {
            await releaseDedupSlot(dedupKey);
            await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'validation_failed', Date.now() - startTime, dedupHash);
            return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: validationError, reqId }) }], isError: true } });
          }
        }

        const circuitKey = resolvedRoute.credentialKey || resolvedServiceName;
        const circuitState = await getCircuitState(circuitKey);
        if (circuitState === 'open') {
          await releaseDedupSlot(dedupKey);
          await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'circuit_open', Date.now() - startTime, dedupHash);
          notifyCircuitOpen(orgId, resolvedServiceName).catch(() => {}); // fire-and-forget, rate-limited internally
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: `Circuit breaker open for ${resolvedServiceName}`, reqId }) }], isError: true } });
        }

        const usageCheck = await checkUsageLimit(orgId);
        if (!usageCheck.ok) {
          await releaseDedupSlot(dedupKey);
          await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'usage_limit_exceeded', Date.now() - startTime, dedupHash);
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: `Monthly usage limit reached (${usageCheck.count}/${usageCheck.limit} actions this month). Contact support@agentraas.io to upgrade.`, reqId }) }], isError: true } });
        }

        const result = await forwardWithRetry(resolvedRoute, resolvedServiceName, resolvedActionName, orgId, payload, reqId, circuitKey);
        recordSuccess(circuitKey).catch(() => {}); // fire-and-forget — closes a half-open circuit on recovery, never blocks the response
        broadcastFanout(resolvedRoute, payload, reqId).catch(() => {}); // fire-and-forget, see broadcastFanout's own error handling
        await completeDedupSlot(dedupKey, idempotencyKey ? { ...result, __payloadDigest: payloadDigest } : result);
        await incrementMonthlyUsage(orgId);
        await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'success', null, Date.now() - startTime, dedupHash, payload);
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ ...result, reqId }) }], isError: false } });
      } catch (err) {
        await releaseDedupSlot(dedupKey);
        if (!err.circuitAlreadyRecorded) await recordFailure(resolvedRoute.credentialKey || resolvedServiceName);
        const upstreamMessage = extractUpstreamErrorMessage(err.response?.data);
        await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'error', upstreamMessage || err.message, Date.now() - startTime, null);
        fastify.log.error({ err, reqId }, 'MCP request failed');
        const responseMessage = err.response
          ? (upstreamMessage || 'Upstream service returned an error.')
          : 'An internal error occurred while processing this request.';
        if (err.response && pg && encryptCredential) {
          pg.query(
            `INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message)
             VALUES ($1, $2, $3, $4, $5, $6, $7)`,
            [reqId, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, encryptCredential(JSON.stringify(payload)), upstreamMessage || err.message]
          ).catch((dlqErr) => fastify.log.warn({ dlqErr, reqId }, 'Dead-letter queue write failed'));
        }
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: responseMessage, reqId }) }], isError: true } });
      }
    }

    return reply.send({ jsonrpc: '2.0', id, error: { code: -32601, message: 'Method not found' } });
  }

  return { handleMCP };
}

module.exports = { createMcp };
