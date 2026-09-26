/**
 * Library mode: exactly-once execution with no AgentRaaS server at all.
 *
 *   import { exactlyOnce, RedisStore } from "agentraas";
 *   import { createClient } from "redis";
 *
 *   const redis = await createClient().connect();
 *   const charge = exactlyOnce(
 *     async (customer: string, amount: number) => (await stripe.charges.create({ customer, amount })).id,
 *     { store: new RedisStore(redis), name: "charge" },
 *   );
 *   await charge("cus_1", 4200); // runs for real
 *   await charge("cus_1", 4200); // retry: cached id, no second charge
 *
 * Same claim/complete/release model as the hosted proxy: the first caller
 * claims a slot, runs the function and stores its result; retries and
 * concurrent duplicates get that result back. If the function throws, the
 * slot is released so a retry can run it for real. Results must be
 * JSON-serializable. `MemoryStore` is single-process only (tests, scripts);
 * use `RedisStore` for anything that restarts or runs more than one process.
 */
import { createHash } from "crypto";

const PENDING = '{"pending":true}';

/** Implement this to back exactlyOnce with any other database. */
export interface DedupStore {
  claim(key: string, ttlSeconds: number): Promise<boolean>;
  get(key: string): Promise<string | null>;
  complete(key: string, value: string, ttlSeconds: number): Promise<void>;
  release(key: string): Promise<void>;
}

export class InFlightError extends Error {}

export class MemoryStore implements DedupStore {
  private m = new Map<string, { v: string; exp: number }>();
  async claim(key: string, ttl: number) {
    const e = this.m.get(key);
    if (e && e.exp > Date.now()) return false;
    this.m.set(key, { v: PENDING, exp: Date.now() + ttl * 1000 });
    return true;
  }
  async get(key: string) {
    const e = this.m.get(key);
    return e && e.exp > Date.now() ? e.v : null;
  }
  async complete(key: string, v: string, ttl: number) {
    this.m.set(key, { v, exp: Date.now() + ttl * 1000 });
  }
  async release(key: string) {
    this.m.delete(key);
  }
}

/** Minimal slice of a node-redis v4+ client (`redis` package). */
interface RedisLike {
  set(key: string, value: string, opts: { NX?: boolean; EX?: number }): Promise<string | null>;
  get(key: string): Promise<string | null>;
  del(key: string): Promise<number>;
}

export class RedisStore implements DedupStore {
  constructor(private r: RedisLike) {}
  async claim(key: string, ttl: number) {
    return (await this.r.set(key, PENDING, { NX: true, EX: ttl })) === "OK";
  }
  get(key: string) {
    return this.r.get(key);
  }
  async complete(key: string, v: string, ttl: number) {
    await this.r.set(key, v, { EX: ttl });
  }
  async release(key: string) {
    await this.r.del(key);
  }
}

export interface ExactlyOnceOptions<A extends unknown[]> {
  store: DedupStore;
  /** Scopes keys; defaults to the function's own name. */
  name?: string;
  /** Idempotency key from the arguments. Default: all arguments, JSON-encoded. */
  key?: (...args: A) => string;
  /** How long a completed result is remembered. Default 24h. */
  ttlSeconds?: number;
  /** How long a duplicate waits for an in-flight first call. Default 30s. */
  waitMs?: number;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Wrap `fn` so identical calls run once. With `passKey: true`, `fn` gets a
 * retry-stable idempotency key as its first argument: pass it to the
 * provider (e.g. Stripe's `idempotencyKey` option). That covers the one
 * case the wrapper can't, where the provider did the work but the response
 * was lost, so `fn` threw and a retry runs it again.
 */
export function exactlyOnce<A extends unknown[], R>(
  fn: (...args: A) => Promise<R> | R,
  opts: ExactlyOnceOptions<A> & { passKey?: false },
): (...args: A) => Promise<R>;
export function exactlyOnce<A extends unknown[], R>(
  fn: (idempotencyKey: string, ...args: A) => Promise<R> | R,
  opts: ExactlyOnceOptions<A> & { passKey: true },
): (...args: A) => Promise<R>;

export function exactlyOnce<A extends unknown[], R>(
  fn: (...args: any[]) => Promise<R> | R,
  opts: ExactlyOnceOptions<A> & { passKey?: boolean },
): (...args: A) => Promise<R> {
  const name = opts.name ?? fn.name;
  if (!name) throw new Error("exactlyOnce: pass `name` for anonymous functions");
  const ttl = opts.ttlSeconds ?? 86400;
  const waitMs = opts.waitMs ?? 30000;
  const { store } = opts;

  return async (...args: A) => {
    const raw = opts.key ? opts.key(...args) : JSON.stringify(args);
    const k = "dedup:local:" + createHash("sha256").update(`${name}\0${raw}`).digest("hex");
    const deadline = Date.now() + waitMs;
    while (!(await store.claim(k, ttl))) {
      const v = await store.get(k);
      if (v !== null && v !== PENDING) return JSON.parse(v).result as R;
      if (Date.now() > deadline) throw new InFlightError(`${name}: duplicate still in flight after ${waitMs}ms`);
      await sleep(100);
    }
    let result: R;
    try {
      result = await (opts.passKey ? fn("ar-" + k.slice("dedup:local:".length, "dedup:local:".length + 48), ...args) : fn(...args));
    } catch (e) {
      await store.release(k);
      throw e;
    }
    const value = JSON.stringify({ result });
    if (value === undefined) {
      await store.release(k);
      throw new TypeError(`exactlyOnce: ${name} returned a non-JSON-serializable result`);
    }
    await store.complete(k, value, ttl);
    return result;
  };
}
