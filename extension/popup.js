const DEFAULT_PORT = 47821;
const enabled = document.getElementById("enabled");
const port = document.getElementById("port");
const status = document.getElementById("status");

async function check() {
  status.className = "";
  status.textContent = "Checking…";
  try {
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 800);
    const res = await fetch(`http://127.0.0.1:${port.value}/ping`, { signal: controller.signal });
    const info = await res.json();
    if (info.app !== "rdm") throw new Error("not RDM");
    status.className = "ok";
    status.textContent = `Connected to RDM ${info.version}`;
  } catch {
    status.className = "bad";
    status.textContent = "RDM is not running (or uses another port). Downloads stay in the browser.";
  }
}

chrome.storage.local.get({ port: DEFAULT_PORT, enabled: true }).then((stored) => {
  enabled.checked = stored.enabled !== false;
  port.value = stored.port;
  check();
});

enabled.addEventListener("change", () => chrome.storage.local.set({ enabled: enabled.checked }));
port.addEventListener("change", () => {
  chrome.storage.local.set({ port: Number(port.value) || DEFAULT_PORT });
  check();
});
