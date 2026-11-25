import { StorageLab } from "./lab-model.mjs";

(() => {
  const $ = (id) => document.getElementById("lab-" + id);
  if (!$("panel")) return;
  const M = StorageLab;
  let pool = M.create();
  let diskId = 1;
  let stripeId = 1;
  let pending = null;
  let stage = 0;
  let events = [];
  let highlighted = new Set();
  const diskName = (id) => "D" + String(id).padStart(2, "0");
  const shardName = (index) => index < pool.k ? "D" + (index + 1) : "P" + (index - pool.k + 1);
  const disk = () => pool.disks.find((d) => d.id === diskId);
  const stripe = () => pool.stripes.find((s) => s.id === stripeId);
  const text = (id, value) => { $(id).textContent = value; };
  function node(tag, className, value) {
    const n = document.createElement(tag);
    n.className = className;
    if (value !== undefined) n.textContent = value;
    return n;
  }
  function log(message, tone = "") {
    events.unshift({ message, tone });
    events = events.slice(0, 3);
    $("announcement").textContent = message;
  }
  function invalidate() {
    pending = null;
    stage = 0;
    highlighted.clear();
  }
  function diskStatus(d) {
    if (!d.online) return "Offline";
    if (d.drain) return M.used(pool, d.id) ? "Draining" : "Drained";
    if (d.health <= 20) return "At risk";
    if (d.slow) return "Slow";
    return "Online";
  }
  function render() {
    const health = M.health(pool);
    const status = health.lost ? "Unreadable" : health.missing ? "Degraded" :
      health.risk ? "At risk" : "Healthy";
    text("health", status);
    $("health").dataset.state = status.toLowerCase().replace(" ", "-");
    text("online", pool.disks.filter((d) => d.online).length + "/" + pool.disks.length);
    text("capacity", pool.stripes.length * (pool.k + pool.m) + "/" +
      pool.disks.reduce((n, d) => n + d.slots, 0));
    text("device-count", pool.disks.length);
    const options = pool.stripes.map((s) => String(s.id));
    if ([...$("stripe").options].map((o) => o.value).join() !== options.join()) {
      $("stripe").replaceChildren(...pool.stripes.map((s) => new Option("#" + s.id, s.id)));
    }
    $("stripe").value = stripeId;
    const selected = stripe();
    const reading = M.read(pool, selected);
    $("stripe-view").style.setProperty("--shards", pool.k + pool.m);
    $("stripe-view").replaceChildren(...selected.shards.map((s) => {
      const device = pool.disks.find((d) => d.id === s.disk);
      const valid = M.available(pool, s);
      const state = valid ? "Valid" : device.online ? "Corrupt" : "Missing";
      const cell = node("button", "lab-shard" + (s.index >= pool.k ? " parity" : ""));
      cell.type = "button";
      cell.dataset.state = state.toLowerCase();
      cell.setAttribute("aria-label", shardName(s.index) + " on " + diskName(s.disk) + ", " + state);
      cell.setAttribute("aria-pressed", String(s.disk === diskId));
      cell.title = state + " · " + s.bytes.map((b) => b.toString(16).padStart(2, "0")).join(" ");
      cell.append(node("strong", "", shardName(s.index)), node("span", "", diskName(s.disk)),
        node("small", "", valid ? "" : state));
      if (highlighted.has(selected.id + ":" + s.index)) cell.classList.add("lab-changed");
      cell.addEventListener("click", () => { diskId = s.disk; render(); $("disks").querySelector('[data-id="' + diskId + '"]').focus(); });
      return cell;
    }));
    text("stripe-status", reading.count + "/" + (pool.k + pool.m) + " valid · " +
      (reading.ok ? reading.count < pool.k + pool.m ? "Recoverable" : "Intact" : "Unreadable"));
    $("stripe-status").dataset.state = reading.ok ? "valid" : "missing";
    const focusedDisk = document.activeElement?.closest("#lab-disks [data-id]")?.dataset.id;
    $("disks").replaceChildren(...pool.disks.map((d) => {
      const button = node("button", "lab-disk");
      button.type = "button";
      button.dataset.id = d.id;
      button.dataset.state = diskStatus(d).toLowerCase().replace(" ", "-");
      button.setAttribute("aria-pressed", String(d.id === diskId));
      const count = M.used(pool, d.id);
      button.setAttribute("aria-label", diskName(d.id) + " " + d.type + ", " + diskStatus(d) +
        ", " + count + " of " + d.slots + " slots");
      const head = node("span", "lab-disk-head");
      head.append(node("strong", "", diskName(d.id)), node("span", "", d.type));
      const bar = node("span", "lab-meter");
      const fill = node("i", "");
      fill.style.width = count / d.slots * 100 + "%";
      bar.append(fill);
      const foot = node("span", "lab-disk-foot");
      foot.append(node("span", "", count + "/" + d.slots), node("span", "", diskStatus(d)));
      button.append(head, bar, foot);
      if (selected.shards.some((s) => s.disk === d.id && highlighted.has(selected.id + ":" + s.index)))
        button.classList.add("lab-changed");
      button.addEventListener("click", () => { diskId = d.id; render(); });
      return button;
    }));
    if (focusedDisk) $("disks").querySelector('[data-id="' + focusedDisk + '"]')?.focus();
    const d = disk();
    text("selection", diskName(d.id) + " · " + d.health + "% health · " +
      (d.latency * (d.slow ? 8 : 1)) + " ms · W " + M.weight(d).toFixed(1));
    text("fail", d.online ? "Fail" : "Restore");
    text("age", d.health === 100 ? "Age" : "Renew");
    text("slow", d.slow ? "Speed up" : "Slow");
    text("drain", d.drain ? "Cancel drain" : "Drain");
    $("age").disabled = $("slow").disabled = $("drain").disabled = !d.online;
    $("corrupt").disabled = !selected.shards.some((s) => s.disk === d.id && M.available(pool, s));
    $("remove").hidden = !d.drain || M.used(pool, d.id) !== 0;
    $("remove").disabled = pool.disks.length <= pool.k + pool.m;
    $("add").disabled = pool.disks.length >= M.MAX_DISKS;
    $("write").disabled = pool.stripes.length >= M.MAX_STRIPES;
    $("apply").disabled = !pending?.verified || !pending.moves.length;
    document.querySelectorAll(".lab-steps li").forEach((li, i) => {
      li.classList.toggle("active", i <= stage);
      if (i === stage) li.setAttribute("aria-current", "step");
      else li.removeAttribute("aria-current");
    });
    $("plan-detail").replaceChildren();
    if (pending) {
      if (pending.moves.length) {
        $("plan-detail").append(node("div", "", pending.moves.length + " moves · " +
          (pending.verified ? "Safety checks passed" : "Blocked")));
        const moveList = node("div", "lab-moves");
        pending.moves.forEach((m) => moveList.append(node("span", "", m.kind + " #" + m.stripe +
          " " + shardName(m.index) + " · " + diskName(m.from) + " → " + diskName(m.to))));
        $("plan-detail").append(moveList);
      }
      if (pending.blocked.length || !pending.moves.length)
        $("plan-detail").append(node("div", "", pending.blocked.join(" · ") || "No maintenance needed"));
    } else text("plan-detail", stage === 3 ? "Batch verified" : "Scan for repairs or at-risk disks.");
    $("events").replaceChildren(...events.map(({ message, tone }) => node("li", tone, message)));
  }
  function mutate(action) {
    invalidate();
    action();
    render();
  }
  function reset() {
    const [k, m] = $("layout").value.split("+").map(Number);
    pool = M.create(k, m);
    diskId = stripeId = 1;
    events = [];
    invalidate();
    log("Pool ready · " + k + "+" + m + " · 4 stripes", "ok");
    render();
  }
  $("reset").addEventListener("click", reset);
  $("layout").addEventListener("change", reset);
  $("stripe").addEventListener("change", () => { stripeId = Number($("stripe").value); render(); });
  $("write").addEventListener("click", () => mutate(() => {
    const written = M.write(pool);
    if (!written) { log("Write blocked · insufficient eligible slots", "warn"); return; }
    stripeId = written.id;
    highlighted = new Set(written.shards.map((s) => written.id + ":" + s.index));
    log("Wrote #" + written.id + " · encoded " + (pool.k + pool.m) + " shards", "ok");
  }));
  $("read").addEventListener("click", () => {
    const result = M.read(pool, stripe());
    log(!result.ok ? "Read #" + stripeId + " failed · insufficient valid shards" :
      "Read #" + stripeId + " · " + (result.count < pool.k + pool.m ? "reconstructed" : "verified") +
      " · " + result.data.flat().length + " bytes", result.ok ? "ok" : "warn");
    render();
  });
  $("add").addEventListener("click", () => mutate(() => {
    const d = M.addDisk(pool, $("media").value);
    if (!d) return;
    diskId = d.id;
    log("Added " + diskName(d.id) + " · " + d.type + " · " + d.slots + " slots", "ok");
  }));
  $("fail").addEventListener("click", () => mutate(() => {
    const d = disk(); d.online = !d.online;
    log(diskName(d.id) + (d.online ? " restored" : " offline"), d.online ? "ok" : "warn");
  }));
  $("corrupt").addEventListener("click", () => mutate(() => {
    if (M.corrupt(pool, stripe(), diskId)) log("Checksum mismatch · #" + stripeId + " / " + diskName(diskId), "warn");
  }));
  $("age").addEventListener("click", () => mutate(() => {
    const d = disk(); d.health = d.health === 100 ? 15 : 100;
    log(diskName(d.id) + " health " + d.health + "%", d.health < 100 ? "warn" : "ok");
  }));
  $("slow").addEventListener("click", () => mutate(() => {
    const d = disk(); d.slow = !d.slow;
    log(diskName(d.id) + (d.slow ? " latency ×8 · placement penalized" : " latency restored"));
  }));
  $("drain").addEventListener("click", () => mutate(() => {
    const d = disk(); d.drain = !d.drain;
    log(diskName(d.id) + (d.drain ? " marked for drain" : " drain cancelled"));
  }));
  $("remove").addEventListener("click", () => mutate(() => {
    if (M.remove(pool, diskId)) {
      log("Removed " + diskName(diskId) + " · no remaining shards", "ok");
      diskId = pool.disks[0].id;
    }
  }));
  function inspect(rebalance) {
    highlighted.clear();
    pending = M.plan(pool, rebalance);
    stage = 1;
    log("Plan · " + pending.moves.length + " moves" +
      (pending.blocked.length ? " · " + pending.blocked.join(", ") : ""));
    render();
  }
  $("plan").addEventListener("click", () => inspect(false));
  $("balance").addEventListener("click", () => inspect(true));
  $("apply").addEventListener("click", () => {
    if (!pending?.verified || !pending.moves.length) return;
    highlighted = new Set(pending.moves.map((m) => m.stripe + ":" + m.index));
    stripeId = pending.moves[0].stripe;
    diskId = pending.moves[0].to;
    pool = pending.next;
    log("Applied " + pending.moves.length + " moves · batch verified", "ok");
    pending = null;
    stage = 3;
    render();
  });
  $("scenario").addEventListener("change", () => {
    const scenario = $("scenario").value;
    if (!scenario) return;
    reset();
    diskId = stripe().shards[0].disk;
    const d = disk();
    if (scenario === "failure") {
      d.online = false;
      log(diskName(d.id) + " offline · try Read", "warn");
    } else if (scenario === "corrupt") {
      M.corrupt(pool, stripe(), diskId);
      log("Checksum mismatch · try Scan & plan", "warn");
    } else if (scenario === "age") {
      d.health = 15;
      log(diskName(d.id) + " health 15% · try Scan & plan", "warn");
    } else if (scenario === "slow") {
      d.slow = true;
      log(diskName(d.id) + " latency ×8 · try Write", "warn");
    } else if (scenario === "expand") {
      for (let i = 0; i < 5; i++) M.write(pool);
      diskId = M.addDisk(pool, "HDD").id;
      log("Capacity expanded · try Rebalance", "ok");
    } else if (scenario === "drain") {
      d.drain = true;
      log(diskName(d.id) + " marked for drain · try Scan & plan");
    }
    $("scenario").value = "";
    render();
  });
  reset();
  $("fallback").hidden = true;
  $("panel").hidden = $("reset").hidden = $("scenario").hidden = false;
})();
