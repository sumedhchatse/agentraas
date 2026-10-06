import { DedupStore, exactlyOnce } from "./local";

type AnyTool = { execute?: (input: any, options: any) => unknown };

export interface ProtectToolsOptions {
  store: DedupStore;
  /** Idempotency key from a call. Default: the tool name plus its whole input. */
  key?: (toolName: string, input: any) => string;
  /** How long a completed result is remembered. Default 24h. */
  ttlSeconds?: number;
  /** How long a duplicate waits for an in-flight first call. Default 30s. */
  waitMs?: number;
}

/**
 * Vercel AI SDK: wrap a `tools` object so each tool runs exactly once per
 * distinct input. When the model calls a tool again with the same input, or
 * a request is retried after a crash, the stored result comes back instead
 * of a second charge or a second email.
 *
 *   const result = await generateText({ model, tools: protectTools({ charge, sendEmail }, { store }) });
 *
 * Use `RedisStore` when more than one process serves requests. Results must
 * be JSON-serializable. Tools without `execute` are passed through.
 */
export function protectTools<T extends Record<string, AnyTool>>(tools: T, opts: ProtectToolsOptions): T {
  const out: Record<string, AnyTool> = {};
  for (const [name, tool] of Object.entries(tools)) {
    const execute = tool.execute;
    if (!execute) {
      out[name] = tool;
      continue;
    }
    // options (toolCallId, messages, abortSignal) differ on every call: passed
    // through, never part of the key.
    const guarded = exactlyOnce((input: unknown, options: unknown) => execute.call(tool, input, options), {
      store: opts.store,
      name: `ai-sdk:${name}`,
      key: (input: unknown) => (opts.key ? opts.key(name, input) : JSON.stringify(input)),
      ttlSeconds: opts.ttlSeconds,
      waitMs: opts.waitMs,
    });
    out[name] = { ...tool, execute: guarded };
  }
  return out as T;
}
