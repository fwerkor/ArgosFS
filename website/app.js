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
