/* A bounded, in-memory teaching model. Slots and risk scores are illustrative. */
export const StorageLab = (() => {
  const media = {
    NVMe: { slots: 8, latency: 1, weight: 3 },
    SSD: { slots: 10, latency: 3, weight: 2 },
    HDD: { slots: 16, latency: 12, weight: 1 },
  };
  const layouts = [[2, 1], [3, 2], [4, 2], [4, 3]];
  const MAX_DISKS = 12;
  const MAX_STRIPES = 20;
  const WIDTH = 4;

  // GF(256), primitive polynomial 0x11d. A systematic Cauchy matrix is MDS:
  // every k rows are invertible, including mixtures of data and parity.
  function mul(a, b) {
    let result = 0;
    while (b) {
      if (b & 1) result ^= a;
      a = (a << 1) ^ (a & 128 ? 0x11d : 0);
      b >>= 1;
    }
    return result;
  }
  function inverse(a) {
    if (!a) throw new Error("Singular matrix");
    let result = 1;
    for (let n = 254; n; n >>= 1, a = mul(a, a)) {
      if (n & 1) result = mul(result, a);
    }
    return result;
  }
  function row(k, index) {
    return Array.from({ length: k }, (_, j) =>
      index < k ? Number(index === j) : inverse((index - k) ^ (j + 16)),
    );
  }
  function combine(coefficients, blocks) {
    return Array.from({ length: WIDTH }, (_, b) =>
      coefficients.reduce((sum, c, i) => sum ^ mul(c, blocks[i][b]), 0),
    );
  }
  function encode(k, m, data) {
    return Array.from({ length: k + m }, (_, i) => combine(row(k, i), data));
  }
  function decode(k, available) {
    if (available.length < k) return null;
    const selected = available.slice(0, k);
    const matrix = selected.map(({ index }, i) => [
      ...row(k, index),
      ...Array.from({ length: k }, (_, j) => Number(i === j)),
    ]);
    for (let col = 0; col < k; col++) {
      const pivot = matrix.findIndex((r, i) => i >= col && r[col]);
      if (pivot < 0) return null;
      [matrix[col], matrix[pivot]] = [matrix[pivot], matrix[col]];
      const factor = inverse(matrix[col][col]);
      matrix[col] = matrix[col].map((v) => mul(v, factor));
      for (let i = 0; i < k; i++) {
        if (i === col) continue;
        const f = matrix[i][col];
        matrix[i] = matrix[i].map((v, j) => v ^ mul(f, matrix[col][j]));
      }
    }
    return matrix.map((r) => combine(r.slice(k), selected.map((s) => s.bytes)));
  }
  // A small deterministic checksum, not the production SHA-256 format.
  function checksum(bytes) {
    return bytes.reduce((h, byte) => Math.imul(h ^ byte, 16777619) >>> 0, 2166136261);
  }
  const copy = (value) => structuredClone(value);
  const diskById = (pool, id) => pool.disks.find((d) => d.id === id);
  const used = (pool, id) => pool.stripes.reduce(
    (sum, s) => sum + s.shards.filter((p) => p.disk === id).length, 0,
  );
  const available = (pool, shard) => diskById(pool, shard.disk)?.online &&
    checksum(shard.bytes) === shard.checksum;
  const good = (pool, stripe) => stripe.shards.filter((s) => available(pool, s));
  const eligible = (pool, d) => d.online && !d.drain && d.health > 20 &&
    used(pool, d.id) < d.slots;
  const weight = (d) => d.weight * d.slots * (d.health / 100) /
    (d.slow ? 8 : 1);
  function rank(pool, key, excluded = []) {
    return pool.disks.filter((d) => eligible(pool, d) && !excluded.includes(d.id))
      .map((d) => {
        const hash = checksum(Array.from(key + ":" + d.id, (c) => c.charCodeAt(0)));
        // Avalanche the hash before mapping to an exponential race.
        let h = hash;
        h = Math.imul(h ^ (h >>> 16), 0x7feb352d);
        h = Math.imul(h ^ (h >>> 15), 0x846ca68b);
        h = (h ^ (h >>> 16)) >>> 0;
        return { d, score: -Math.log((h + 1) / 4294967297) / weight(d) };
      }).sort((a, b) => a.score - b.score || a.d.id - b.d.id).map(({ d }) => d);
  }
  function addDisk(pool, type) {
    if (!media[type] || pool.disks.length >= MAX_DISKS) return null;
    const d = { id: pool.nextDisk++, type, ...media[type], online: true,
      health: 100, slow: false, drain: false };
    pool.disks.push(d);
    return d;
  }
  function write(pool) {
    if (pool.stripes.length >= MAX_STRIPES) return null;
    const id = pool.nextStripe;
    const targets = rank(pool, "stripe-" + id).slice(0, pool.k + pool.m);
    if (targets.length < pool.k + pool.m) return null;
    const data = Array.from({ length: pool.k }, (_, i) =>
      Array.from({ length: WIDTH }, (_, j) => (id * 37 + i * 71 + j * 19) & 255),
    );
    const stripe = { id, checksum: checksum(data.flat()),
      shards: encode(pool.k, pool.m, data).map((bytes, index) =>
        ({ index, disk: targets[index].id, bytes, checksum: checksum(bytes) })) };
    pool.nextStripe++;
    pool.stripes.push(stripe);
    return stripe;
  }
  function create(k = 4, m = 2) {
    if (!layouts.some(([a, b]) => a === k && b === m)) throw new Error("Invalid layout");
    const pool = { k, m, disks: [], stripes: [], nextDisk: 1, nextStripe: 1 };
    const types = ["NVMe", "SSD", "HDD", "NVMe", "SSD", "HDD", "HDD", "SSD"];
    types.slice(0, k + m + 1).forEach((type) => addDisk(pool, type));
    for (let i = 0; i < 4; i++) write(pool);
    return pool;
  }
  function read(pool, stripe) {
    const surviving = good(pool, stripe);
    const data = decode(pool.k, surviving);
    return { count: surviving.length, ok: !!data && checksum(data.flat()) === stripe.checksum,
      data };
  }
  function corrupt(pool, stripe, id) {
    const shard = stripe.shards.find((s) => s.disk === id);
    if (!shard || !available(pool, shard)) return false;
    shard.bytes[0] ^= 0xff;
    return true;
  }
  function health(pool) {
    const missing = pool.stripes.reduce((n, s) => n + s.shards.length - good(pool, s).length, 0);
    const lost = pool.stripes.filter((s) => !read(pool, s).ok).length;
    return { missing, lost, risk: pool.disks.filter((d) => (d.health <= 20 || d.drain) && used(pool, d.id)).length };
  }
  // A plan is calculated on a copy, with a fixed move budget. Never overwrite
  // a good shard before a replacement is available and verified.
  function plan(pool, rebalance = false) {
    const next = copy(pool);
    const moves = [];
    const blocked = new Set();
    const budget = 4;
    function move(stripe, shard, target, bytes, kind) {
      moves.push({ stripe: stripe.id, index: shard.index, from: shard.disk, to: target.id, kind });
      Object.assign(shard, { disk: target.id, bytes, checksum: checksum(bytes) });
    }
    const endangered = (d) => d.drain || d.health <= 20;
    const result = (s) => read(next, s);
    // Recover missing/corrupt shards before optional migration.
    for (const stripe of next.stripes) {
      const reading = result(stripe);
      if (!reading.ok) { blocked.add("Unreadable stripe"); continue; }
      const encoded = encode(next.k, next.m, reading.data);
      for (const shard of stripe.shards) {
        if (moves.length >= budget || available(next, shard)) continue;
        const source = diskById(next, shard.disk);
        const target = source.online && !endangered(source) ? source :
          rank(next, "repair-" + stripe.id + "-" + shard.index, stripe.shards.map((s) => s.disk))[0];
        if (!target) { blocked.add("No spare slot"); continue; }
        move(stripe, shard, target, encoded[shard.index], "Repair");
      }
    }
    // Do not drain or rebalance while any stripe is still degraded.
    if (!health(next).missing) {
      for (const stripe of next.stripes) {
        for (const shard of stripe.shards) {
          if (moves.length >= budget || !endangered(diskById(next, shard.disk))) continue;
          const target = rank(next, "drain-" + stripe.id + "-" + shard.index,
            stripe.shards.map((s) => s.disk))[0];
          if (!target) { blocked.add("Drain needs spare capacity"); continue; }
          move(stripe, shard, target, shard.bytes, "Drain");
        }
      }
      if (rebalance && !next.disks.some((d) => endangered(d) && used(next, d.id))) {
        for (const stripe of next.stripes) {
          for (const shard of stripe.shards) {
            if (moves.length >= budget) break;
            if (moves.some((m) => m.stripe === stripe.id && m.index === shard.index)) continue;
            const source = diskById(next, shard.disk);
            const fill = (d) => used(next, d.id) / d.slots;
            const target = rank(next, "balance-" + stripe.id,
              stripe.shards.map((s) => s.disk)).sort((a, b) => fill(a) - fill(b))[0];
            if (target && fill(source) - fill(target) > 1 / source.slots + 1 / target.slots)
              move(stripe, shard, target, shard.bytes, "Balance");
          }
        }
      }
    }
    return { next, moves, blocked: [...blocked], verified: next.stripes.every((s) =>
      read(next, s).ok || !read(pool, pool.stripes.find((old) => old.id === s.id)).ok) &&
      next.stripes.every((s) => new Set(s.shards.map((p) => p.disk)).size === next.k + next.m) &&
      next.disks.every((d) => used(next, d.id) <= d.slots) };
  }
  function remove(pool, id) {
    const d = diskById(pool, id);
    if (!d || !d.drain || used(pool, id) || pool.disks.length <= pool.k + pool.m) return false;
    pool.disks = pool.disks.filter((disk) => disk.id !== id);
    return true;
  }
  return { media, layouts, MAX_DISKS, MAX_STRIPES, create, addDisk, write, read,
    corrupt, health, plan, remove, used, available, good, eligible, weight, encode, decode };
})();
