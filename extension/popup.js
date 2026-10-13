"use strict";

const status = document.getElementById("status");
const enabled = document.getElementById("enabled");
const videoButton = document.getElementById("videoButton");
const mediaSection = document.getElementById("media-section");
const mediaList = document.getElementById("media");

function setStatus(kind, text) {
  status.className = kind;
  status.textContent = text;
}

function formatSize(bytes) {
  if (!bytes) return "";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 100 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

function describe(url) {
  try {
    const parsed = new URL(url);
    const leaf = parsed.pathname.split("/").filter(Boolean).pop() || "";
    return { name: decodeURIComponent(leaf) || parsed.hostname, host: parsed.hostname };
  } catch {
    return { name: url, host: "" };
  }
}

async function showStatus() {
  const reply = await chrome.runtime.sendMessage({ type: "status" }).catch(() => null);
  if (!reply?.ok) return setStatus("bad", "The extension is still starting. Open this again in a moment.");
  enabled.checked = reply.settings.enabled;
  videoButton.checked = reply.settings.videoButton;
  if (!reply.status.installed) {
    setStatus("bad", "Rapid Download Manager is not connected to this browser. Install it (or run its installer again), then restart the browser. Until then downloads stay in the browser.");
  } else if (reply.status.running) {
    setStatus("ok", `Connected to RDM ${reply.status.version || ""}`.trim());
  } else {
    setStatus("idle", "RDM is installed and will start when a download needs it.");
  }
}

async function showMedia() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return;
  const reply = await chrome.runtime.sendMessage({ type: "media", tabId: tab.id }).catch(() => null);
  const media = Array.isArray(reply?.media) ? reply.media : [];
  mediaSection.hidden = media.length === 0;
  mediaList.replaceChildren();
  for (const item of media) {
    const { name, host } = describe(item.url);
    const kind = item.kind === "stream" ? "Stream (HLS)" : item.kind === "audio" ? "Audio" : "Video";

    const row = document.createElement("li");
    const text = document.createElement("div");
    text.className = "text";
    const title = document.createElement("div");
    title.className = "name";
    title.textContent = item.label || name;
    title.title = item.url;
    const meta = document.createElement("div");
    meta.className = "meta";
    meta.textContent = [item.label ? "" : kind, formatSize(item.size), host].filter(Boolean).join(" · ");
    text.append(title, meta);

    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Download";
    button.addEventListener("click", async () => {
      button.disabled = true;
      const result = await chrome.runtime.sendMessage({ type: "download", url: item.url, tabId: tab.id }).catch(() => null);
      button.textContent = result?.ok ? "Sent" : "Failed";
      button.title = result?.ok ? "" : result?.error || "Rapid Download Manager could not be reached";
      if (!result?.ok) setTimeout(() => ((button.disabled = false), (button.textContent = "Download")), 2000);
    });
    row.append(text, button);
    mediaList.append(row);
  }
}

enabled.addEventListener("change", () => chrome.storage.local.set({ enabled: enabled.checked }));
videoButton.addEventListener("change", () => chrome.storage.local.set({ videoButton: videoButton.checked }));

showStatus();
showMedia();
