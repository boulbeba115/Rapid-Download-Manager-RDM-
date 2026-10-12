// Rapid Download Manager — browser integration (Chrome / Edge, Manifest V3).
//
// When the browser starts a download whose file type RDM is configured to
// capture, the download is cancelled in the browser and handed to RDM through
// its loopback API, together with the referrer and cookies it needs to fetch
// the same file. If RDM is not running, the browser keeps the download.

const DEFAULT_PORT = 47821;
const FALLBACK_EXTENSIONS = [
  "zip", "rar", "7z", "gz", "bz2", "xz", "tar", "iso", "tgz", "zst",
  "exe", "msi", "msix", "apk", "dmg", "bin", "deb", "rpm",
  "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "epub",
  "mp3", "wav", "flac", "aac", "m4a", "ogg", "opus",
  "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "m4v",
];

async function settings() {
  const stored = await chrome.storage.local.get({ port: DEFAULT_PORT, enabled: true });
  return { port: Number(stored.port) || DEFAULT_PORT, enabled: stored.enabled !== false };
}

function base(port) {
  return `http://127.0.0.1:${port}`;
}

async function call(port, path, options = {}, timeoutMs = 1500) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(base(port) + path, { ...options, signal: controller.signal });
    if (!res.ok) throw new Error(`RDM answered ${res.status}`);
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}

// The capture list lives in RDM's options; cache it briefly.
let cachedExtensions = null;
let cachedAt = 0;
async function captureExtensions(port) {
  if (cachedExtensions && Date.now() - cachedAt < 60_000) return cachedExtensions;
  try {
    const cfg = await call(port, "/config", {}, 800);
    cachedExtensions = new Set((cfg.captureExtensions || []).map((e) => e.toLowerCase()));
  } catch {
    cachedExtensions = new Set(FALLBACK_EXTENSIONS);
  }
  cachedAt = Date.now();
  return cachedExtensions;
}

function extensionOf(nameOrUrl) {
  if (!nameOrUrl) return "";
  let name = nameOrUrl;
  try {
    name = new URL(nameOrUrl).pathname;
  } catch {
    // Not a URL: treat as a file path.
  }
  const leaf = name.split(/[\\/]/).pop() || "";
  const dot = leaf.lastIndexOf(".");
  return dot > 0 ? leaf.slice(dot + 1).toLowerCase() : "";
}

function leafName(path) {
  return path ? path.split(/[\\/]/).pop() : undefined;
}

async function cookieHeader(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

async function sendToRdm(port, { url, referer, fileName }) {
  const body = {
    url,
    referer: referer || undefined,
    cookies: (await cookieHeader(url)) || undefined,
    // The cookies belong to this browser session, so the request should look like it.
    userAgent: navigator.userAgent,
    fileName: fileName || undefined,
  };
  await call(port, "/add", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

chrome.downloads.onCreated.addListener(async (item) => {
  try {
    const { port, enabled } = await settings();
    if (!enabled) return;
    const url = item.finalUrl || item.url;
    if (!/^https?:\/\//i.test(url)) return; // blob:, data:, file: stay in the browser

    const wanted = await captureExtensions(port);
    const ext = extensionOf(item.filename) || extensionOf(url);
    if (!wanted.has(ext)) return;

    // Only take the download away from the browser if RDM is really there.
    await call(port, "/ping", {}, 600);
    await sendToRdm(port, { url, referer: item.referrer, fileName: leafName(item.filename) });

    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch {
    // RDM is not running or refused: leave the browser's own download alone.
  }
});

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "rdm-download-link",
    title: "Download with RDM",
    contexts: ["link", "video", "audio", "image"],
  });
});

chrome.contextMenus.onClicked.addListener(async (info) => {
  if (info.menuItemId !== "rdm-download-link") return;
  const url = info.linkUrl || info.srcUrl;
  if (!url || !/^https?:\/\//i.test(url)) return;
  const { port } = await settings();
  try {
    await sendToRdm(port, { url, referer: info.pageUrl });
  } catch {
    // Nothing useful to do from a service worker; the popup shows whether RDM is reachable.
  }
});
