/**
 * AgentRaaS TypeScript/JavaScript SDK
 * Exactly-once execution for AI agents — a thin wrapper around the
 * AgentRaaS SDK-style REST gateway (POST /v1/sdk/:service/:action).
 *
 * Quickstart:
 *   import { Client } from "agentraas";
 *
 *   const client = new Client({
 *     agentraasKey: "ar_live_...",   // from the dashboard's Connect Agent panel
 *     orgId: "acme-corp",             // optional, but strongly recommended (see below)
 *     agentId: "billing-bot",         // optional, but strongly recommended (see below)
 *   });
 *   const result = await client.call("stripe", "charge.create", { amount: 5000, currency: "usd" });
 *
 * Why orgId/agentId matter: without them, every SDK caller that omits
 * them shares one unenforced "sdk"/"sdk-agent" identity server-side —
 * your per-agent rate limit and audit trail won't distinguish your
 * agents from anyone else's untagged SDK traffic. Always set them once
 * you've connected an agent from the dashboard.
 *
 * Retries are safe: AgentRaaS claims an atomic dedup slot server-side
 * before forwarding anything, so retrying call() after a network error
 * (timeout, connection drop) never double-executes the real action —
 * the retry either completes normally or gets the cached result back.
 * This client does not retry automatically; your own retry logic (or
 * your agent framework's) is safe to use as-is.
 */

export interface ClientOptions {
  /** Your AgentRaaS agent API key (ar_live_... — from the dashboard's Connect Agent panel). */
  agentraasKey: string;
  /** Your org id. Optional but recommended — see module docstring. */
  orgId?: string;
  /** Your agent id. Optional but recommended — see module docstring. */
  agentId?: string;
  /**
   * Your AgentRaaS deployment's base URL, e.g. "https://your-deployment.example.com"
   * or "http://localhost:13000" for a local self-hosted instance (the default).
   */
  baseUrl?: string;
  /** Per-request timeout in milliseconds (default 30000). */
  timeoutMs?: number;
}

export class AgentRaaSError extends Error {
  statusCode?: number;
  reqId?: string;

  constructor(message: string, statusCode?: number, reqId?: string) {
    super(reqId ? `${message} (reqId=${reqId})` : message);
    this.name = 'AgentRaaSError';
    this.statusCode = statusCode;
    this.reqId = reqId;
  }
}

const DEFAULT_BASE_URL = 'http://localhost:13000';

export class Client {
  private readonly agentraasKey: string;
  private readonly orgId?: string;
  private readonly agentId?: string;
  private readonly baseUrl: string;
  private readonly timeoutMs: number;

  constructor(options: ClientOptions) {
    if (!options.agentraasKey) throw new Error('agentraasKey is required');
    this.agentraasKey = options.agentraasKey;
    this.orgId = options.orgId;
    this.agentId = options.agentId;
    this.baseUrl = (options.baseUrl || DEFAULT_BASE_URL).replace(/\/$/, '');
    this.timeoutMs = options.timeoutMs ?? 30000;
  }

  private headers(): Record<string, string> {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      'X-AgentRaaS-Key': this.agentraasKey,
    };
    if (this.orgId) headers['X-AgentRaaS-Org'] = this.orgId;
    if (this.agentId) headers['X-AgentRaaS-Agent'] = this.agentId;
    return headers;
  }

  /**
   * Make one protected request.
   *
   * @param service Curated service name (stripe, twilio, hubspot, ... see
   *   your dashboard's Services list) or "custom" for a Custom Action.
   * @param action Dotted action name for a curated service (e.g.
   *   "charge.create"), or your Custom Action's registered name.
   * @param payload Request body, forwarded to the upstream API.
   * @returns The upstream response body (or the cached result, with
   *   `cached: true`, if this exact request already ran).
   * @throws {AgentRaaSError} on any non-2xx response.
   */
  async call<T = any>(service: string, action: string, payload: Record<string, unknown> = {}): Promise<T> {
    const url = `${this.baseUrl}/v1/sdk/${encodeURIComponent(service)}/${encodeURIComponent(action)}`;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);

    let response: Response;
    try {
      response = await fetch(url, {
        method: 'POST',
        headers: this.headers(),
        body: JSON.stringify(payload),
        signal: controller.signal,
      });
    } catch (err) {
      throw new AgentRaaSError(`Could not reach AgentRaaS at ${this.baseUrl}: ${(err as Error).message}`);
    } finally {
      clearTimeout(timer);
    }

    const data = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new AgentRaaSError(data.error || `HTTP ${response.status}`, response.status, data.reqId);
    }
    return data as T;
  }

  /** Shorthand for call("custom", action, payload) — calls a registered Custom Action by name. */
  custom<T = any>(action: string, payload: Record<string, unknown> = {}): Promise<T> {
    return this.call<T>('custom', action, payload);
  }

  /** Returns a proxy for dot-notation calls: client.service("stripe").call("charge.create", payload). */
  service(name: string): { call: <T = any>(action: string, payload?: Record<string, unknown>) => Promise<T> } {
    return { call: (action, payload = {}) => this.call(name, action, payload) };
  }
}

export default Client;
