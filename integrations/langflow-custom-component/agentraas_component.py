"""AgentRaaS custom component for Langflow.

Exactly-once execution for any API call in a Langflow flow — wraps a call
with AgentRaaS's atomic dedup guarantee, so a re-run or a retried step in
the flow never double-charges, double-books, or double-sends anything.

Install: drop this file into Langflow's custom components directory
(the path set by LANGFLOW_COMPONENTS_PATH, or Langflow's default
~/.langflow/components) and restart Langflow — it appears in the
component sidebar under "AgentRaaS".

Verification note: written to Langflow's documented Component API (the
langflow.custom.Component base, as of Langflow 1.x) but not load-tested
against a running Langflow instance in this repo — please open an issue
if something doesn't match your version.
"""

import json

import httpx
from langflow.custom import Component
from langflow.io import Output, SecretStrInput, StrInput
from langflow.schema import Data


class AgentRaaSComponent(Component):
    display_name = "AgentRaaS"
    description = (
        "Exactly-once execution for any API call — use instead of a raw HTTP "
        "request whenever the action has a real-world side effect (charges, "
        "bookings, messages) that must never happen twice."
    )
    icon = "shield"
    name = "AgentRaaS"

    inputs = [
        StrInput(
            name="base_url",
            display_name="AgentRaaS Base URL",
            value="http://localhost:13000",
            info="Your AgentRaaS deployment — self-hosted or your Cloud/deployment URL.",
            required=True,
        ),
        SecretStrInput(
            name="agent_key",
            display_name="Agent API Key",
            info='From the dashboard\'s "+ Connect Agent" panel (ar_live_...).',
            required=True,
        ),
        StrInput(
            name="org_id",
            display_name="Org ID",
            info="Recommended — without this, requests fall back to an unenforced shared identity.",
            required=False,
        ),
        StrInput(
            name="agent_id",
            display_name="Agent ID",
            info="Recommended alongside Org ID — same identity you connected the agent under.",
            required=False,
        ),
        StrInput(
            name="service",
            display_name="Service",
            info="A curated AgentRaaS service (see your dashboard's Services list), or \"custom\".",
            required=True,
        ),
        StrInput(
            name="action",
            display_name="Action",
            info="Dotted action name (e.g. charge.create), or your Custom Action's registered name.",
            required=True,
        ),
        StrInput(
            name="payload",
            display_name="Payload (JSON)",
            value="{}",
            info="Request body forwarded to the upstream API, as a JSON string.",
            required=True,
        ),
    ]

    outputs = [
        Output(display_name="Result", name="result", method="call_agentraas"),
    ]

    def call_agentraas(self) -> Data:
        headers = {"X-AgentRaaS-Key": self.agent_key}
        if self.org_id:
            headers["X-AgentRaaS-Org"] = self.org_id
        if self.agent_id:
            headers["X-AgentRaaS-Agent"] = self.agent_id

        try:
            payload = json.loads(self.payload or "{}")
        except json.JSONDecodeError as err:
            raise ValueError(f"payload must be valid JSON: {err}") from err

        url = f"{self.base_url.rstrip('/')}/v1/sdk/{self.service}/{self.action}"
        try:
            response = httpx.post(url, headers=headers, json=payload, timeout=30)
        except httpx.HTTPError as err:
            raise ValueError(f"Could not reach AgentRaaS at {self.base_url}: {err}") from err

        data = response.json()
        if response.status_code >= 400:
            raise ValueError(data.get("error", f"AgentRaaS request failed ({response.status_code})"))

        return Data(data=data)
