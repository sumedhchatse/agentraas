"""
AgentRaaS Python SDK
Exactly-once execution for AI agents — a thin wrapper around the
AgentRaaS SDK-style REST gateway (POST /v1/sdk/:service/:action).

Quickstart:
    import agentraas

    client = agentraas.Client(
        agentraas_key="ar_live_...",   # from the dashboard's Connect Agent panel
        org_id="acme-corp",             # optional, but strongly recommended (see below)
        agent_id="billing-bot",         # optional, but strongly recommended (see below)
    )
    result = client.call("stripe", "charge.create", {"amount": 5000, "currency": "usd"})

    # or, per-service sugar:
    stripe = client.service("stripe")
    result = stripe.charge.create({"amount": 5000, "currency": "usd"})

Why org_id/agent_id matter: without them, every SDK caller that omits
them shares one unenforced "sdk"/"sdk-agent" identity server-side — your
per-agent rate limit and audit trail won't distinguish your agents from
anyone else's untagged SDK traffic. Always set them once you've
connected an agent from the dashboard.

Retries are safe: AgentRaaS claims an atomic dedup slot server-side
before forwarding anything, so retrying a `call()` after a network
error (timeout, connection drop) never double-executes the real
action — the retry either completes normally or gets the cached
result back. This client does not retry automatically; your own
retry logic (or your agent framework's) is safe to use as-is.
"""

import requests

__version__ = "0.2.0"

DEFAULT_BASE_URL = "http://localhost:13000"


class AgentRaaSError(Exception):
    """Raised for any non-2xx response from AgentRaaS itself."""

    def __init__(self, message, status_code=None, req_id=None):
        super().__init__(message)
        self.status_code = status_code
        self.req_id = req_id

    def __str__(self):
        base = super().__str__()
        if self.status_code:
            base = f"[{self.status_code}] {base}"
        if self.req_id:
            base = f"{base} (reqId={self.req_id})"
        return base


class _ServiceProxy:
    """Returned by Client.service(name) — lets you write stripe.charge.create(...)
    instead of client.call('stripe', 'charge.create', ...)."""

    def __init__(self, client, service):
        self._client = client
        self._service = service

    def __getattr__(self, prefix):
        return _ActionProxy(self._client, self._service, prefix)


class _ActionProxy:
    def __init__(self, client, service, prefix):
        self._client = client
        self._service = service
        self._prefix = prefix

    def __getattr__(self, suffix):
        action = f"{self._prefix}.{suffix}"
        return lambda payload=None: self._client.call(self._service, action, payload)


class Client:
    """A reusable AgentRaaS client. One Client per agent identity — create
    it once and reuse it for every call, rather than per-request."""

    def __init__(self, agentraas_key, org_id=None, agent_id=None, base_url=None, timeout=30):
        """
        Args:
            agentraas_key: Your AgentRaaS agent API key (ar_live_... — from
                the dashboard's Connect Agent panel).
            org_id: Your org id. Optional but recommended — see module docstring.
            agent_id: Your agent id. Optional but recommended — see module docstring.
            base_url: Your AgentRaaS deployment's base URL, e.g.
                "https://your-deployment.example.com" or
                "http://localhost:13000" for a local self-hosted instance
                (the default). Cloud users: use your AgentRaaS Cloud URL.
            timeout: Per-request timeout in seconds (default 30).
        """
        if not agentraas_key:
            raise ValueError("agentraas_key is required")
        self.agentraas_key = agentraas_key
        self.org_id = org_id
        self.agent_id = agent_id
        self.base_url = (base_url or DEFAULT_BASE_URL).rstrip("/")
        self.timeout = timeout
        self._session = requests.Session()

    def _headers(self):
        headers = {
            "Content-Type": "application/json",
            "X-AgentRaaS-Key": self.agentraas_key,
        }
        if self.org_id:
            headers["X-AgentRaaS-Org"] = self.org_id
        if self.agent_id:
            headers["X-AgentRaaS-Agent"] = self.agent_id
        return headers

    def call(self, service, action, payload=None):
        """Make one protected request.

        Args:
            service: Curated service name (stripe, twilio, hubspot, ...
                see your dashboard's Services list) or "custom" for a
                Custom Action you've registered.
            action: Action name — for curated services, the dotted
                action from that service's docs (e.g. "charge.create");
                for service="custom", the Custom Action's registered name.
            payload: Request body dict, forwarded to the upstream API.

        Returns:
            The upstream response body as a dict (or the cached result,
            with "cached": true, if this exact request already ran).

        Raises:
            AgentRaaSError: on any non-2xx response.
        """
        url = f"{self.base_url}/v1/sdk/{service}/{action}"
        try:
            response = self._session.post(
                url, headers=self._headers(), json=payload or {}, timeout=self.timeout
            )
        except requests.RequestException as err:
            raise AgentRaaSError(f"Could not reach AgentRaaS at {self.base_url}: {err}") from err

        if response.status_code >= 400:
            try:
                error_data = response.json()
            except ValueError:
                error_data = {}
            raise AgentRaaSError(
                error_data.get("error", f"HTTP {response.status_code}"),
                status_code=response.status_code,
                req_id=error_data.get("reqId"),
            )

        return response.json()

    def custom(self, action, payload=None):
        """Shorthand for call("custom", action, payload) — calls one of
        your registered Custom Actions by name."""
        return self.call("custom", action, payload)

    def service(self, name):
        """Returns a proxy for dot-notation calls: client.service("stripe").charge.create(payload)."""
        return _ServiceProxy(self, name)

    def close(self):
        self._session.close()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()


def protect(service, agentraas_key, org_id=None, agent_id=None, base_url=None):
    """Deprecated convenience shim for pre-0.2 callers. Prefer Client(...).service(name).

    Usage:
        import agentraas
        stripe = agentraas.protect("stripe", agentraas_key="ar_live_...")
        result = stripe.request("charge.create", {"amount": 5000, "currency": "usd"})
    """
    client = Client(agentraas_key, org_id=org_id, agent_id=agent_id, base_url=base_url)
    return _LegacyProxy(client, service)


class _LegacyProxy:
    def __init__(self, client, service):
        self._client = client
        self._service = service

    def request(self, action, payload=None):
        return self._client.call(self._service, action, payload)


__all__ = ["Client", "AgentRaaSError", "protect"]
