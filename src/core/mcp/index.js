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
    VALIDATION_RULES,
    validatePayload,
    resolveCustomRoute,
    verifyApiKey,
    getEffectiveRateLimit,
    checkUsageLimit,
    incrementMonthlyUsage,
    logAudit,
    extractUpstreamErrorMessage,
  } = deps;
  const {
    generateRequestId,
    hashPayload,
    checkAgentRateLimit,
    claimDedupSlot,
    readDedupSlot,
    completeDedupSlot,
    releaseDedupSlot,
    getCircuitState,
    recordFailure,
    forwardAction,
  } = proxy;

  async function handleMCP(request, reply) {
    const { jsonrpc, method, params, id } = request.body || {};
    if (jsonrpc !== '2.0') return reply.status(400).send({ jsonrpc: '2.0', error: { code: -32600, message: 'Invalid Request' }, id: id || null });

    if (method === 'tools/list') {
      const tools = Object.entries(SERVICE_CONFIG).flatMap(([svcName, svc]) =>
        Object.entries(svc.actions).map(([actName, act]) => ({
          name: `${svcName}_${actName.replace('.', '_')}`,
          description: `AgentRaaS-protected ${svcName} ${actName}`,
          inputSchema: {
            type: 'object',
            properties: {
              payload: { type: 'object', description: 'Request payload' },
              org_id: { type: 'string', description: 'Organization ID' },
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
      const apiKey = request.headers['x-agentraas-key'] || 'anonymous';
      const reqId = generateRequestId();

      if (!toolName || typeof toolName !== 'string') {
        return reply.send({ jsonrpc: '2.0', id, error: { code: -32602, message: 'Invalid params: "name" is required and must be a string.' } });
      }

      const parts = toolName.split('_');
      const actionName = parts.pop().replace(/_/g, '.');
      const serviceName = parts.join('_');
      const routeKey = `${serviceName}.${actionName}`;

      let resolvedRoute = SERVICE_ROUTES[routeKey];
      let resolvedServiceName = serviceName;
      let resolvedActionName = actionName;
      if (!resolvedRoute) {
        // Fall back: treat the whole tool name as a registered custom action name
        // (custom action names may contain underscores, which breaks the
        // split-on-last-underscore heuristic used for built-in service.action tools).
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
      const dedupHash = hashPayload(apiKey, resolvedServiceName, resolvedActionName, payload);
      const { key: dedupKey, claimed } = await claimDedupSlot(dedupHash);

      if (!claimed) {
        const existing = await readDedupSlot(dedupKey);
        if (!existing || existing.pending) {
          await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'duplicate_in_progress', Date.now() - startTime, dedupHash);
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: 'An identical request is already being processed. Retry shortly.', reqId }) }], isError: true } });
        }
        await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'deduplicated', null, Date.now() - startTime, dedupHash);
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ ...existing, cached: true, reqId }) }], isError: false } });
      }

      try {
        if (resolvedServiceName !== 'custom') {
          const validationError = validatePayload(resolvedServiceName, resolvedActionName, payload, VALIDATION_RULES);
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
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: `Circuit breaker open for ${resolvedServiceName}`, reqId }) }], isError: true } });
        }

        const usageCheck = await checkUsageLimit(orgId);
        if (!usageCheck.ok) {
          await releaseDedupSlot(dedupKey);
          await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'blocked', 'usage_limit_exceeded', Date.now() - startTime, dedupHash);
          return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: `Monthly usage limit reached (${usageCheck.count}/${usageCheck.limit} actions this month). Contact support@agentraas.io to upgrade.`, reqId }) }], isError: true } });
        }

        const result = await forwardAction(resolvedRoute, resolvedServiceName, resolvedActionName, orgId, payload, reqId);
        await completeDedupSlot(dedupKey, result);
        await incrementMonthlyUsage(orgId);
        await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'success', null, Date.now() - startTime, dedupHash, payload);
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ ...result, reqId }) }], isError: false } });
      } catch (err) {
        await releaseDedupSlot(dedupKey);
        await recordFailure(resolvedRoute.credentialKey || resolvedServiceName);
        const upstreamMessage = extractUpstreamErrorMessage(err.response?.data);
        await logAudit(reqId, apiKey, orgId, 'mcp-agent', resolvedServiceName, resolvedActionName, 'error', upstreamMessage || err.message, Date.now() - startTime, null);
        fastify.log.error({ err, reqId }, 'MCP request failed');
        const responseMessage = err.response
          ? (upstreamMessage || 'Upstream service returned an error.')
          : 'An internal error occurred while processing this request.';
        return reply.send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: JSON.stringify({ error: responseMessage, reqId }) }], isError: true } });
      }
    }

    return reply.send({ jsonrpc: '2.0', id, error: { code: -32601, message: 'Method not found' } });
  }

  return { handleMCP };
}

module.exports = { createMcp };
