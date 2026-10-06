"""
CrewAI: exactly-once tool calls.

    from agentraas.crewai import protect_tool

    agent = Agent(role=..., tools=[protect_tool(ChargeCardTool()), protect_tool(send_email)])

A protected tool runs once per distinct set of arguments: when the agent
calls it again with the same arguments, or a crew is re-kicked off after a
crash, the stored result comes back instead of a second charge or a second
email. Same guarantee and stores as `agentraas.local.exactly_once` (SQLite by
default, `RedisStore` across machines); needs `pip install "agentraas[crewai]"`.
"""

import json

from crewai.tools import BaseTool
from crewai.tools.base_tool import Tool

from .local import DEFAULT_TTL, exactly_once

__all__ = ["protect_tool"]


def protect_tool(tool: BaseTool, store=None, key=None, ttl=DEFAULT_TTL, wait=30.0) -> BaseTool:
    """Return a copy of `tool` (a BaseTool subclass or an `@tool` function) whose calls run exactly once.

    key: optional function taking the tool's arguments as keywords and
         returning the idempotency key (e.g. `key=lambda order_id, **_: order_id`).
         Default: the tool name plus all arguments.
    ttl: how long a result is remembered; an identical call inside this
         window is treated as a duplicate, not a new action.

    Tool results must be JSON-serializable (strings, dicts, lists).
    """
    raw_key = key or (lambda *a, **kw: [a, kw])

    def slot(*a, **kw):
        return json.dumps([tool.name, raw_key(*a, **kw)], sort_keys=True, default=str)

    guard = exactly_once(store=store, key=slot, ttl=ttl, wait=wait)

    if isinstance(tool, Tool):
        # An @tool function: run() calls .func directly, skipping _run, and
        # _run/_arun call it too, so guarding .func covers every path.
        return tool.model_copy(update={"func": guard(tool.func)})

    # A BaseTool subclass: everything (run, arun, and to_structured_tool,
    # which the Agent uses) goes through _run/_arun. They call the ORIGINAL
    # tool: its own _arun may fall back to _run, which on the copy is already
    # guarded, and nesting the guard would deadlock.
    def run(*a, **kw):
        return tool._run(*a, **kw)

    async def arun(*a, **kw):
        return await tool._arun(*a, **kw)

    # Same name so a sync call and an async call share one dedup slot.
    arun.__qualname__ = run.__qualname__

    protected = tool.model_copy()
    # object.__setattr__: pydantic would refuse the assignment.
    object.__setattr__(protected, "_run", guard(run))
    object.__setattr__(protected, "_arun", guard(arun))
    return protected
