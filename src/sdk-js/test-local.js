// Run: npm run build && node test-local.js
const assert = require("assert");
const { exactlyOnce, MemoryStore } = require("./dist");
(async () => {
  let calls = 0;
  const charge = exactlyOnce(async (c) => { calls++; await new Promise((r) => setTimeout(r, 200)); return { id: "ch_" + calls }; }, { store: new MemoryStore(), name: "charge" });
  const r = await Promise.all(Array.from({ length: 8 }, () => charge("cus_1")));
  assert(calls === 1 && r.every((x) => x.id === "ch_1"), "8 concurrent calls run once");
  await charge("cus_2");
  assert.strictEqual(calls, 2, "different args run separately");
  let n = 0;
  const flaky = exactlyOnce(async () => { if (++n === 1) throw new Error("timeout"); return "ok"; }, { store: new MemoryStore(), name: "flaky" });
  await flaky().catch(() => {});
  assert(await flaky() === "ok" && n === 2, "failure releases the slot");
  // passKey: same key on every retry of the same call, different key for a different call
  const seen = [];
  const refund = exactlyOnce(async (key, charge) => { seen.push(key); if (seen.length === 1) throw new Error("lost response"); return "re_1"; },
    { store: new MemoryStore(), name: "refund", passKey: true });
  await refund("ch_1").catch(() => {});
  assert.strictEqual(await refund("ch_1"), "re_1");
  assert(seen[0] === seen[1] && seen[0].startsWith("ar-"), "key stable across retries");
  await refund("ch_2");
  assert.notStrictEqual(seen[2], seen[0], "different call, different key");
  console.log("ok");
})();
