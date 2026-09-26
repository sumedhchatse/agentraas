"""Run: python test_local.py  (no dependencies)"""
import asyncio
import os
import tempfile
import threading
import time

from agentraas.local import InFlightError, SQLiteStore, exactly_once


def store():
    return SQLiteStore(os.path.join(tempfile.mkdtemp(), "d.db"))


def test_retry_returns_cached_result():
    calls = []

    @exactly_once(store=store())
    def charge(customer, amount):
        calls.append(1)
        return {"id": f"ch_{len(calls)}"}

    assert charge("cus_1", 42) == {"id": "ch_1"}
    assert charge("cus_1", amount=42) == {"id": "ch_1"}  # same args, different spelling
    assert charge("cus_2", 42) == {"id": "ch_2"}
    assert len(calls) == 2


def test_eight_concurrent_calls_execute_once():
    calls = []

    @exactly_once(store=store())
    def charge(customer):
        calls.append(1)
        time.sleep(0.3)
        return "ch_1"

    results = []
    threads = [threading.Thread(target=lambda: results.append(charge("cus_1"))) for _ in range(8)]
    [t.start() for t in threads]
    [t.join() for t in threads]
    assert len(calls) == 1 and results == ["ch_1"] * 8


def test_failure_releases_slot():
    calls = []

    @exactly_once(store=store())
    def flaky():
        calls.append(1)
        if len(calls) == 1:
            raise TimeoutError
        return "ok"

    try:
        flaky()
    except TimeoutError:
        pass
    assert flaky() == "ok" and len(calls) == 2


def test_custom_key_and_ttl_expiry():
    calls = []

    @exactly_once(store=store(), key=lambda order, note=None: order["id"], ttl=1)
    def ship(order, note=None):
        calls.append(1)
        return len(calls)

    assert ship({"id": 7}, note="a") == 1
    assert ship({"id": 7}, note="b") == 1  # key ignores note
    time.sleep(1.1)
    assert ship({"id": 7}) == 2  # window expired


def test_in_flight_duplicate_times_out():
    s = store()

    @exactly_once(store=s, wait=0.2)
    def slow():
        time.sleep(1)
        return 1

    t = threading.Thread(target=slow)
    t.start()
    time.sleep(0.1)
    try:
        slow()
        assert False, "expected InFlightError"
    except InFlightError:
        pass
    t.join()


def test_async_and_non_json_result():
    calls = []

    @exactly_once(store=store())
    async def send(to):
        calls.append(1)
        await asyncio.sleep(0.1)
        return "sent"

    async def main():
        return await asyncio.gather(*[send("a@b.c") for _ in range(5)])

    assert asyncio.run(main()) == ["sent"] * 5 and len(calls) == 1

    @exactly_once(store=store())
    def bad():
        return object()

    try:
        bad()
        assert False
    except TypeError:
        pass


def test_idempotency_key_is_injected_and_stable_across_retries():
    seen = []

    @exactly_once(store=store())
    def charge(customer, idempotency_key=None):
        seen.append(idempotency_key)
        if len(seen) == 1:
            raise ConnectionError  # provider did the work, response lost
        return "ch_1"

    try:
        charge("cus_1")
    except ConnectionError:
        pass
    assert charge("cus_1") == "ch_1"
    assert seen[0] == seen[1] and seen[0].startswith("ar-")
    assert charge("cus_2") == "ch_1" and seen[2] != seen[0]  # different call, different key


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
