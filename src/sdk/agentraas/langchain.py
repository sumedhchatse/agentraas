"""
LangChain / LangGraph: exactly-once tool calls.

    from agentraas.langchain import protect_tool

    tools = [protect_tool(t) for t in [charge_card, send_email]]
    agent = create_react_agent(model, tools)      # or ToolNode(tools)

A protected tool runs once per distinct set of arguments: when the model
calls it again with the same arguments, or the graph re-runs the tool node
after a crash or a retry policy fires, the stored result comes back instead
of a second charge or a second email. Same guarantee and stores as
`agentraas.local.exactly_once` (SQLite by default, `RedisStore` across
machines); needs `pip install "agentraas[langchain]"`.
"""

import json

from langchain_core.runnables import RunnableConfig
from langchain_core.tools import BaseTool, StructuredTool

from .local import DEFAULT_TTL, exactly_once

__all__ = ["protect_tool"]


def protect_tool(tool: BaseTool, store=None, key=None, ttl=DEFAULT_TTL, wait=30.0) -> BaseTool:
    """Return a copy of `tool` whose calls run exactly once.

    key: optional function taking the tool's arguments as keywords and
         returning the idempotency key (e.g. `key=lambda order_id, **_: order_id`).
         Default: the tool name plus all arguments.
    ttl: how long a result is remembered; an identical call inside this
         window is treated as a duplicate, not a new action.

    Tool results must be JSON-serializable (strings, dicts, lists).
    """
    raw_key = key or (lambda **kw: kw)

    def slot(config=None, **kw):
        # config carries callbacks/run ids that change per run: never part of the key.
        return json.dumps([tool.name, raw_key(**kw)], sort_keys=True, default=str)

    def run(config: RunnableConfig = None, **kw):
        return tool.invoke(kw, config=config)

    async def arun(config: RunnableConfig = None, **kw):
        return await tool.ainvoke(kw, config=config)

    # Same name so a sync call and an async call share one dedup slot.
    arun.__qualname__ = run.__qualname__
    guard = exactly_once(store=store, key=slot, ttl=ttl, wait=wait)

    return StructuredTool.from_function(
        func=guard(run),
        coroutine=guard(arun),
        name=tool.name,
        description=tool.description,
        args_schema=tool.args_schema,
        return_direct=tool.return_direct,
    )
