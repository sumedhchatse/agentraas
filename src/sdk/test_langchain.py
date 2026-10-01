"""Run: pip install langchain-core langgraph && python test_langchain.py"""
import asyncio
import os
import tempfile

from langchain_core.messages import AIMessage
from langchain_core.tools import tool
from langgraph.prebuilt import ToolNode

from agentraas.langchain import protect_tool
from agentraas.local import SQLiteStore

calls = []


@tool
def charge(customer: str, amount: int) -> str:
    """Charge a customer."""
    calls.append((customer, amount))
    return f"ch_{len(calls)}"


def store():
    return SQLiteStore(os.path.join(tempfile.mkdtemp(), "d.db"))


def test_duplicate_tool_call_runs_once():
    calls.clear()
    t = protect_tool(charge, store=store())
    assert t.name == "charge" and t.args == charge.args
    assert t.invoke({"customer": "cus_1", "amount": 42}) == "ch_1"
    assert t.invoke({"customer": "cus_1", "amount": 42}) == "ch_1"
    assert asyncio.run(t.ainvoke({"customer": "cus_1", "amount": 42})) == "ch_1"  # async shares the slot
    assert t.invoke({"customer": "cus_2", "amount": 42}) == "ch_2"
    assert len(calls) == 2


def test_langgraph_tool_node_rerun_runs_once():
    calls.clear()
    node = ToolNode([protect_tool(charge, store=store())])
    msg = AIMessage("", tool_calls=[{"name": "charge", "args": {"customer": "cus_1", "amount": 5}, "id": "call_1"}])
    for _ in range(3):  # graph resumed/retried three times
        out = node.invoke({"messages": [msg]})
        assert out["messages"][0].content == "ch_1"
    assert len(calls) == 1


def test_custom_key_and_failure_releases_slot():
    attempts = []

    @tool
    def ship(order_id: str, note: str) -> str:
        """Ship an order."""
        attempts.append(order_id)
        if len(attempts) == 1:
            raise ConnectionError("provider down")
        return "shipped"

    t = protect_tool(ship, store=store(), key=lambda order_id, **_: order_id)
    try:
        t.invoke({"order_id": "o1", "note": "a"})
    except ConnectionError:
        pass
    assert t.invoke({"order_id": "o1", "note": "a"}) == "shipped"  # failure didn't cache
    assert t.invoke({"order_id": "o1", "note": "different"}) == "shipped"  # keyed on order id only
    assert len(attempts) == 2


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
