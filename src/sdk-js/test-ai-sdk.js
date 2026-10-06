// Run: npm run build && npm i --no-save ai zod && node test-ai-sdk.js
const assert = require("assert");
const { generateText, tool } = require("ai");
const { MockLanguageModelV3 } = require("ai/test");
const { z } = require("zod");
const { protectTools, MemoryStore } = require("./dist");

// A model that always asks for the same charge, with a fresh toolCallId each time,
// like a model repeating itself or a request retried after a crash.
let n = 0;
const model = () => new MockLanguageModelV3({
  doGenerate: async () => ({
    content: [{ type: "tool-call", toolCallId: `call_${++n}`, toolName: "charge", input: JSON.stringify({ customer: "cus_1", amount: 42 }) }],
    finishReason: { unified: "tool-calls", raw: "tool_calls" },
    usage: { inputTokens: { total: 1 }, outputTokens: { total: 1 } },
    warnings: [],
  }),
});

(async () => {
  let charges = 0;
  const seenCallIds = [];
  const tools = protectTools({
    charge: tool({
      description: "Charge a customer",
      inputSchema: z.object({ customer: z.string(), amount: z.number() }),
      execute: async ({ customer }, { toolCallId }) => { charges++; seenCallIds.push(toolCallId); return { id: `ch_${charges}`, customer }; },
    }),
  }, { store: new MemoryStore() });

  const results = await Promise.all([1, 2, 3].map(() => generateText({ model: model(), tools, prompt: "charge cus_1" })));
  for (const r of results) assert.deepStrictEqual(r.toolResults[0].output, { id: "ch_1", customer: "cus_1" });
  assert.strictEqual(charges, 1, "three identical tool calls, one charge");
  assert.ok(seenCallIds[0].startsWith("call_"), "options still reach execute");
  console.log("ok ai-sdk");
})().catch((e) => { console.error(e); process.exit(1); });
