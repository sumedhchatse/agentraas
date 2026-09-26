"""
Library mode: exactly-once execution with no AgentRaaS server at all.

    from agentraas.local import exactly_once

    @exactly_once()                       # SQLite file in the working dir
    def charge(customer, amount):
        return stripe.Charge.create(customer=customer, amount=amount).id

    charge("cus_1", 4200)   # runs for real
    charge("cus_1", 4200)   # retry: returns the cached id, no second charge

Same guarantee as the hosted proxy, same claim/complete/release model as
`crates/core/src/dedup.rs`: the first caller atomically claims a slot,
runs the function and stores its result; any retry or concurrent duplicate
gets that result back instead of running it again. If the function
raises, the slot is released so a retry can run it for real.

Stores: `SQLiteStore` (default, stdlib only, safe across threads and
processes on one machine) and `RedisStore` (needs `pip install redis`,
safe across machines). Results must be JSON-serializable.
"""

import asyncio
import functools
import hashlib
import inspect
import json
import sqlite3
import time

__all__ = ["exactly_once", "SQLiteStore", "RedisStore", "InFlightError"]

DEFAULT_TTL = 86400  # 24h, same as the server's default dedup window
_PENDING = '{"pending":true}'


class InFlightError(Exception):
    """A duplicate call arrived while the first one was still running and
    didn't finish within `wait` seconds. Retry later; it is not safe to
    assume the first call failed."""


class SQLiteStore:
    def __init__(self, path="agentraas-dedup.db"):
        self.path = path
        with self._conn() as c:
            c.execute("CREATE TABLE IF NOT EXISTS dedup (key TEXT PRIMARY KEY, value TEXT NOT NULL, expires_at REAL NOT NULL)")

    def _conn(self):
        # A connection per operation keeps this safe to share across threads.
        return sqlite3.connect(self.path, timeout=30, isolation_level=None)

    def claim(self, key, ttl):
        now = time.time()
        c = self._conn()
        try:
            c.execute("BEGIN IMMEDIATE")
            c.execute("DELETE FROM dedup WHERE key = ? AND expires_at < ?", (key, now))
            cur = c.execute("INSERT OR IGNORE INTO dedup VALUES (?, ?, ?)", (key, _PENDING, now + ttl))
            c.execute("COMMIT")
            return cur.rowcount == 1
        finally:
            c.close()

    def get(self, key):
        with self._conn() as c:
            row = c.execute("SELECT value FROM dedup WHERE key = ? AND expires_at >= ?", (key, time.time())).fetchone()
        return row[0] if row else None

    def complete(self, key, value, ttl):
        with self._conn() as c:
            c.execute("UPDATE dedup SET value = ?, expires_at = ? WHERE key = ?", (value, time.time() + ttl, key))

    def release(self, key):
        with self._conn() as c:
            c.execute("DELETE FROM dedup WHERE key = ?", (key,))


class RedisStore:
    def __init__(self, url="redis://localhost:6379/0", client=None):
        if client is None:
            import redis  # optional dependency
            client = redis.Redis.from_url(url)
        self.r = client

    def claim(self, key, ttl):
        return bool(self.r.set(key, _PENDING, nx=True, ex=ttl))

    def get(self, key):
        v = self.r.get(key)
        return v.decode() if isinstance(v, bytes) else v

    def complete(self, key, value, ttl):
        self.r.set(key, value, ex=ttl)

    def release(self, key):
        self.r.delete(key)


def _default_key(fn, args, kwargs):
    bound = inspect.signature(fn).bind_partial(*args, **kwargs)
    bound.apply_defaults()
    bound.arguments.pop("idempotency_key", None)
    return json.dumps(bound.arguments, sort_keys=True, default=str)


def exactly_once(store=None, key=None, ttl=DEFAULT_TTL, wait=30.0, poll=0.1):
    """Decorate a function (sync or async) so identical calls run once.

    key:  optional function taking the same arguments and returning the
          idempotency key (e.g. `key=lambda order, **_: order["id"]`).
          Default: all arguments, JSON-encoded.
    ttl:  how long a completed result is remembered, in seconds.
    wait: how long a duplicate waits for an in-flight first call.

    If the function has an `idempotency_key` parameter, it is filled with a
    key that stays the same across retries. Pass it to the provider (e.g.
    Stripe's Idempotency-Key header): that covers the one case this
    decorator can't, where the provider did the work but the response was
    lost, so the function raised and a retry runs it again.
    """
    store = store or SQLiteStore()

    def decorate(fn):
        name = f"{fn.__module__}.{fn.__qualname__}"
        wants_key = "idempotency_key" in inspect.signature(fn).parameters

        def slot(args, kwargs):
            raw = key(*args, **kwargs) if key else _default_key(fn, args, kwargs)
            return "dedup:local:" + hashlib.sha256(f"{name}\0{raw}".encode()).hexdigest()

        def inject(k, kwargs):
            if wants_key and kwargs.get("idempotency_key") is None:
                kwargs["idempotency_key"] = "ar-" + k[len("dedup:local:"):][:48]
            return kwargs

        def cached(k):
            v = store.get(k)
            if v is None or v == _PENDING:
                return v, None
            return v, json.loads(v)["result"]

        def finish(k, result):
            try:
                value = json.dumps({"result": result})
            except TypeError:
                store.release(k)
                raise TypeError(f"@exactly_once: {name} returned a non-JSON-serializable result; return plain data (ids, dicts)")
            store.complete(k, value, ttl)
            return result

        if inspect.iscoroutinefunction(fn):
            @functools.wraps(fn)
            async def async_wrapper(*args, **kwargs):
                k = slot(args, kwargs)
                deadline = time.monotonic() + wait
                while not store.claim(k, ttl):
                    v, result = cached(k)
                    if v not in (None, _PENDING):
                        return result
                    if time.monotonic() > deadline:
                        raise InFlightError(f"{name}: duplicate still in flight after {wait}s")
                    await asyncio.sleep(poll)
                try:
                    result = await fn(*args, **inject(k, kwargs))
                except BaseException:
                    store.release(k)
                    raise
                return finish(k, result)
            return async_wrapper

        @functools.wraps(fn)
        def wrapper(*args, **kwargs):
            k = slot(args, kwargs)
            deadline = time.monotonic() + wait
            while not store.claim(k, ttl):
                v, result = cached(k)
                if v not in (None, _PENDING):
                    return result
                if time.monotonic() > deadline:
                    raise InFlightError(f"{name}: duplicate still in flight after {wait}s")
                time.sleep(poll)
            try:
                result = fn(*args, **inject(k, kwargs))
            except BaseException:
                store.release(k)
                raise
            return finish(k, result)
        return wrapper

    return decorate
