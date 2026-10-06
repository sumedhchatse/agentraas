"""Run: pip install crewai && python test_crewai.py  (CrewAI needs Python 3.10+)"""
import asyncio
import os
import tempfile

from crewai.tools import BaseTool, tool

from agentraas.crewai import protect_tool
from agentraas.local import SQLiteStore

calls = []


class ChargeTool(BaseTool):
    name: str = "charge"
    description: str = "Charge a customer."

    def _run(self, customer: str, amount: int) -> str:
        calls.append((customer, amount))
        return f"ch_{len(calls)}"


def store():
    return SQLiteStore(os.path.join(tempfile.mkdtemp(), "d.db"))


def test_duplicate_tool_call_runs_once():
    calls.clear()
    t = protect_tool(ChargeTool(), store=store())
    assert t.name == "charge"
    assert t.run(customer="cus_1", amount=42) == "ch_1"
    assert t.run(customer="cus_1", amount=42) == "ch_1"
    if hasattr(type(t), "arun"):  # async tools: CrewAI 1.x later releases
        assert asyncio.run(t.arun(customer="cus_1", amount=42)) == "ch_1"  # async shares the slot
    assert t.run(customer="cus_2", amount=42) == "ch_2"
    assert len(calls) == 2


def test_agent_path_is_protected_and_original_is_untouched():
    calls.clear()
    original = ChargeTool()
    t = protect_tool(original, store=store())
    structured = t.to_structured_tool()  # what a CrewAI Agent actually calls
    for _ in range(3):  # crew re-kicked off three times
        assert structured.invoke({"customer": "cus_1", "amount": 5}) == "ch_1"
    assert len(calls) == 1
    original.run(customer="cus_1", amount=5)  # the unwrapped tool still runs every time
    assert len(calls) == 2


def test_tool_decorator_custom_key_and_failure_releases_slot():
    attempts = []

    @tool("ship")
    def ship(order_id: str, note: str) -> str:
        """Ship an order."""
        attempts.append(order_id)
        if len(attempts) == 1:
            raise ConnectionError("provider down")
        return "shipped"

    t = protect_tool(ship, store=store(), key=lambda order_id, **_: order_id)
    try:
        t.run(order_id="o1", note="a")
    except ConnectionError:
        pass
    assert t.run(order_id="o1", note="a") == "shipped"  # failure didn't cache
    assert t.run(order_id="o1", note="different") == "shipped"  # keyed on order id only
    assert len(attempts) == 2
    assert t.to_structured_tool().invoke({"order_id": "o1", "note": "b"}) == "shipped"  # agent path too
    assert len(attempts) == 2


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
