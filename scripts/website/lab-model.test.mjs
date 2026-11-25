import { test } from "node:test";
import assert from "node:assert/strict";
import { StorageLab as M } from "../../website/lab-model.mjs";

for (const [k, m] of M.layouts) {
  test(k + "+" + m + " reconstructs from every survivor subset of at least k", () => {
    const pool = M.create(k, m);
    const stripe = pool.stripes[0];
    const data = M.read(pool, stripe).data;
    for (let mask = 0; mask < 1 << (k + m); mask++) {
      const subset = stripe.shards.filter((s) => mask & (1 << s.index));
      assert.deepEqual(M.decode(k, subset), subset.length < k ? null : data);
    }
  });
}
test("corruption is detected, reconstructed and repaired in place", () => {
  const p = M.create();
  const s = p.stripes[0];
  const expected = M.read(p, s).data;
  assert(M.corrupt(p, s, s.shards[0].disk));
  assert.equal(M.good(p, s).length, 5);
  assert.deepEqual(M.read(p, s).data, expected);
  const before = structuredClone(p);
  const plan = M.plan(p);
  assert.deepEqual(p, before);
  assert(plan.verified);
  assert.equal(plan.moves.length, 1);
  assert.equal(plan.moves[0].from, plan.moves[0].to);
  assert.equal(M.health(plan.next).missing, 0);
});
test("disk failure relocates only onto distinct available disks", () => {
  let p = M.create();
  const id = p.stripes[0].shards[0].disk;
  p.disks.find(d => d.id === id).online = false;
  assert(M.health(p).missing > 0);
  const plan = M.plan(p);
  assert(plan.verified);
  assert(plan.moves.every(m => m.from === id && m.to !== id));
  assert.equal(M.health(plan.next).missing, 0);
});
test("unrecoverable stripes and exhausted spare capacity are not fabricated", () => {
  const p = M.create();
  const s = p.stripes[0];
  s.shards.slice(0, 3).forEach(shard => {
    p.disks.find(d => d.id === shard.disk).online = false;
  });
  assert.equal(M.read(p, s).ok, false);
  const plan = M.plan(p, true);
  assert(plan.blocked.includes("Unreadable stripe"));
  assert(!plan.moves.some(m => m.stripe === s.id));
  assert.equal(M.write(p), null);
});
test("two missing shards with one spare defer remaining repair", () => {
  const p = M.create();
  const s = p.stripes[0];
  s.shards.slice(0, 2).forEach(shard => {
    p.disks.find(d => d.id === shard.disk).online = false;
  });
  const plan = M.plan(p);
  assert(plan.verified);
  assert(plan.blocked.includes("No spare slot"));
  assert(M.health(plan.next).missing > 0);
  assert(M.read(plan.next, plan.next.stripes[0]).ok);
});
test("aging, slowdown, expansion, drain and safe removal", () => {
  let p = M.create();
  const d = p.disks.find(d => d.id === p.stripes[0].shards[0].disk);
  const weight = M.weight(d);
  d.slow = true;
  assert(M.weight(d) < weight);
  d.health = 15;
  assert(!M.eligible(p, d));
  const added = M.addDisk(p, "SSD");
  assert(added);
  const plan = M.plan(p);
  assert(plan.moves.some(m => m.kind === "Drain"));
  assert(plan.verified);
  p = plan.next;
  assert.equal(M.used(p, d.id), 0);
  const target = p.disks.find(x => x.id === d.id);
  target.drain = true;
  assert(M.remove(p, d.id));
  assert(!p.disks.some(x => x.id === d.id));
  assert.equal(M.health(p).missing, 0);
});
test("rebalancing respects capacity, unique placement and bounded batches", () => {
  let p = M.create();
  while (M.write(p)) {}
  const added = M.addDisk(p, "HDD");
  const before = structuredClone(p);
  const plan = M.plan(p, true);
  assert.deepEqual(p, before);
  assert(plan.verified);
  assert(plan.moves.length > 0 && plan.moves.length <= 4);
  assert(M.used(plan.next, added.id) > 0);
  assert(plan.next.disks.every(d => M.used(plan.next, d.id) <= d.slots));
  assert.equal(M.health(plan.next).lost, 0);
});
test("capacity and device limits fail without mutation", () => {
  const p = M.create(2, 1);
  while (M.addDisk(p, "NVMe")) {}
  assert.equal(p.disks.length, M.MAX_DISKS);
  while (M.write(p)) {}
  const before = structuredClone(p);
  assert.equal(M.write(p), null);
  assert.equal(M.addDisk(p, "SSD"), null);
  assert.deepEqual(p, before);
});
