// Navigation stays visible when JavaScript is unavailable.
const header = document.querySelector(".site-header");
const toggle = document.querySelector(".nav-toggle");
const nav = document.querySelector(".site-nav");

if (header && toggle && nav) {
  const narrow = window.matchMedia("(max-width: 900px)");
  const setOpen = (open) => {
    nav.classList.toggle("open", open);
    toggle.setAttribute("aria-expanded", String(open));
    toggle.innerHTML = open
      ? 'Close <span aria-hidden="true">−</span>'
      : 'Menu <span aria-hidden="true">+</span>';
  };
  const updateLayout = () => {
    setOpen(false);
    toggle.hidden = !narrow.matches;
    header.classList.toggle("menu-ready", narrow.matches);
  };
  toggle.addEventListener("click", () => {
    setOpen(toggle.getAttribute("aria-expanded") !== "true");
  });
  header.addEventListener("keydown", (event) => {
    if (
      event.key === "Escape" &&
      toggle.getAttribute("aria-expanded") === "true"
    ) {
      setOpen(false);
      toggle.focus();
    }
  });
  nav.addEventListener("click", (event) => {
    if (event.target.closest("a")) setOpen(false);
  });
  document.addEventListener("click", (event) => {
    if (!header.contains(event.target)) setOpen(false);
  });
  narrow.addEventListener("change", updateLayout);
  updateLayout();
}

// This illustrates the recovery threshold of one 4+2 stripe.
// It does not simulate placement, run an encoder, or read a real volume.
const disks = [...document.querySelectorAll(".disk")];
const reset = document.querySelector("#reset-disks");
const state = document.querySelector(".recovery-state");
const title = document.querySelector("#recovery-title");
const detail = document.querySelector("#recovery-detail");

if (disks.length === 6 && reset && state && title && detail) {
  const updateStripe = () => {
    const missing = disks.filter(
      (disk) => disk.getAttribute("aria-pressed") === "true",
    ).length;
    const available = disks.length - missing;
    reset.hidden = missing === 0;
    state.dataset.state =
      missing > 2 ? "lost" : missing ? "degraded" : "healthy";
    title.textContent =
      missing > 2
        ? "Not enough shards to recover"
        : missing
          ? "Stripe remains recoverable"
          : "All six shards available";
    detail.textContent =
      missing > 2
        ? `${available} of 6 available; at least 4 are required.`
        : missing
          ? `${available} of 6 available. Up to ${2 - missing} more can be lost.`
          : "Any four are enough to recover this stripe.";
  };
  disks.forEach((disk, index) => {
    disk.disabled = false;
    disk.addEventListener("click", () => {
      const failed = disk.getAttribute("aria-pressed") !== "true";
      disk.setAttribute("aria-pressed", String(failed));
      disk.setAttribute(
        "aria-label",
        failed
          ? `Restore disk ${index + 1}`
          : `Simulate failure of disk ${index + 1}`,
      );
      disk.querySelector(".disk-state").textContent = failed
        ? "Missing"
        : "Online";
      updateStripe();
    });
  });
  reset.addEventListener("click", () => {
    disks.forEach((disk, index) => {
      disk.setAttribute("aria-pressed", "false");
      disk.setAttribute("aria-label", `Simulate failure of disk ${index + 1}`);
      disk.querySelector(".disk-state").textContent = "Online";
    });
    updateStripe();
    disks[0].focus();
  });
  document.querySelector("#diagram-hint").textContent =
    "Click a disk to take its shard offline.";
}

// The code remains selectable if the browser cannot write to the clipboard.
document.querySelectorAll(".copy-button").forEach((button) => {
  if (!window.isSecureContext || !navigator.clipboard?.writeText) return;
  const code = button.closest(".code-block").querySelector("code");
  button.hidden = false;
  button.setAttribute("aria-label", "Copy commands");
  button.setAttribute("aria-live", "polite");
  button.addEventListener("click", async () => {
    button.disabled = true;
    try {
      await navigator.clipboard.writeText(code.textContent);
      button.textContent = "Copied";
    } catch {
      button.textContent = "Select code to copy";
      const selection = window.getSelection();
      const range = document.createRange();
      range.selectNodeContents(code);
      selection.removeAllRanges();
      selection.addRange(range);
    }
    window.setTimeout(() => {
      button.textContent = "Copy";
      button.disabled = false;
    }, 2000);
  });
});
