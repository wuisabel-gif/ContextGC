(() => {
  const copyButtons = document.querySelectorAll("[data-copy]");
  const tabs = document.querySelectorAll("[data-os]");
  const panels = document.querySelectorAll("[data-os-panel]");
  const steps = document.querySelectorAll("[data-step]");
  const menuButton = document.querySelector(".menu-toggle");
  const nav = document.querySelector(".site-nav");

  async function copyText(text, button) {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const area = document.createElement("textarea");
      area.value = text;
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    const label = button.querySelector("span") || button;
    const original = label.textContent;
    label.textContent = "Copied";
    button.classList.add("copied");
    window.setTimeout(() => {
      label.textContent = original;
      button.classList.remove("copied");
    }, 1600);
  }

  copyButtons.forEach((button) => {
    button.addEventListener("click", () => copyText(button.dataset.copy || "", button));
  });

  function selectOs(os) {
    tabs.forEach((tab) => {
      const active = tab.dataset.os === os;
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-selected", String(active));
    });
    panels.forEach((panel) => {
      const active = panel.dataset.osPanel === os;
      panel.hidden = !active;
      panel.classList.toggle("active", active);
    });
  }

  tabs.forEach((tab) => tab.addEventListener("click", () => selectOs(tab.dataset.os)));

  steps.forEach((step) => {
    step.addEventListener("click", () => {
      steps.forEach((candidate) => candidate.classList.toggle("active", candidate === step));
      const target = document.querySelector(`[data-step-block="${step.dataset.step}"]`);
      if (target) target.scrollIntoView({ behavior: "smooth", block: "center" });
    });
  });

  if (menuButton && nav) {
    menuButton.addEventListener("click", () => {
      const open = nav.classList.toggle("open");
      menuButton.setAttribute("aria-expanded", String(open));
    });
    nav.querySelectorAll("a").forEach((link) => {
      link.addEventListener("click", () => {
        nav.classList.remove("open");
        menuButton.setAttribute("aria-expanded", "false");
      });
    });
  }
})();
