// Rapid Download Manager — browser integration (Chrome / Edge, Manifest V3).
//
// Three jobs:
//
//  1. Downloads. When the browser is about to save a file of a type RDM is
//     configured to capture, the browser's copy is held, RDM is asked to take
//     it (with the referrer and cookies it needs), and only when RDM has
//     accepted is the browser's copy cancelled. If RDM cannot take it, the
//     browser carries on as if nothing had happened — never both, never neither.
//
//  2. Media. Responses that are video or audio files, or HLS playlists, are
//     noted per tab so the page button (content.js) and the popup can offer
//     them. Facebook feeds its player in pieces from script; there the pieces
//     say which video is playing, and the address of the whole file comes from
//     the page's own data (page-hook.js).
//
//  3. Talking to RDM. Only through native messaging: the browser starts
//     RDM's host program and lets this extension — and nothing else, no web
//     page, no other extension — exchange messages with it. This extension
//     makes no network requests of its own.

"use strict";

// Must match HOST_NAME in crates/rdm-core/src/browser.rs.
const HOST = "io.github.boulbeba115.rdm";

// Used only when RDM's own list cannot be read.
const FALLBACK_EXTENSIONS = [
  "zip", "rar", "7z", "gz", "bz2", "xz", "tar", "iso", "tgz", "zst",
  "exe", "msi", "msix", "apk", "dmg", "bin", "deb", "rpm",
  "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "epub",
  "mp3", "wav", "flac", "aac", "m4a", "ogg", "opus",
  "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "m4v",
];

const MEDIA_EXTENSIONS = new Set([
  "mp4", "m4v", "webm", "mkv", "mov", "avi", "wmv", "flv", "mpg", "mpeg", "ogv",
  "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "flac",
]);
// Pieces of a segmented stream: useless on their own.
const SEGMENT_EXTENSIONS = new Set(["ts", "m4s", "m4f", "cmfv", "cmfa", "frag"]);
const PLAYLIST_TYPES = new Set(["application/vnd.apple.mpegurl", "application/x-mpegurl", "audio/mpegurl", "audio/x-mpegurl"]);
const MIN_MEDIA_BYTES = 100 * 1024;
const MAX_MEDIA_PER_TAB = 12;
// Video sites whose terms forbid downloading (and whose streams are not plain files anyway).
const EXCLUDED_HOSTS = /(^|\.)(youtube\.com|youtu\.be|youtube-nocookie\.com|googlevideo\.com|ytimg\.com)$/i;

const isWeb = (url) => typeof url === "string" && /^https?:\/\//i.test(url);
const quiet = (promise) => Promise.resolve(promise).catch(() => undefined);

function hostOf(url) {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
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

async function settings() {
  const stored = await chrome.storage.local.get({ enabled: true, videoButton: true });
  return { enabled: stored.enabled !== false, videoButton: stored.videoButton !== false };
}

// ---------------------------------------------------------------- RDM

// What is known about RDM, cached briefly: asking starts the host program.
let cachedStatus = null;
let cachedAt = 0;

async function rdmStatus(maxAgeMs = 30_000) {
  if (cachedStatus && Date.now() - cachedAt < maxAgeMs) return cachedStatus;
  try {
    const reply = await chrome.runtime.sendNativeMessage(HOST, { type: "status" });
    const list = Array.isArray(reply?.captureExtensions) ? reply.captureExtensions : FALLBACK_EXTENSIONS;
    cachedStatus = {
      installed: true,
      running: reply?.running === true,
      version: typeof reply?.version === "string" ? reply.version : null,
      hostVersion: typeof reply?.hostVersion === "string" ? reply.hostVersion : null,
      extensions: list.filter((e) => typeof e === "string").map((e) => e.toLowerCase()),
    };
  } catch (error) {
    // The host is not registered (RDM not installed, or installed for another browser profile).
    cachedStatus = { installed: false, running: false, version: null, hostVersion: null, extensions: [], error: String(error?.message || error) };
  }
  cachedAt = Date.now();
  return cachedStatus;
}

async function cookieHeader(url, incognito) {
  try {
    const details = { url };
    if (incognito) {
      // A private window has its own cookie jar; never mix the normal one into it.
      const stores = await chrome.cookies.getAllCookieStores();
      const store = stores.find((s) => s.id !== "0");
      if (!store) return "";
      details.storeId = store.id;
    }
    const cookies = await chrome.cookies.getAll(details);
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch {
    return "";
  }
}

// Resolves when RDM has accepted the link; rejects otherwise.
async function sendToRdm({ url, referer, fileName, incognito }) {
  if (!isWeb(url)) throw new Error("Not a web address");
  const reply = await chrome.runtime.sendNativeMessage(HOST, {
    type: "add",
    url,
    referer: isWeb(referer) ? referer : undefined,
    cookies: (await cookieHeader(url, incognito === true)) || undefined,
    // The cookies belong to this browser session, so the request should look like it.
    userAgent: navigator.userAgent,
    fileName: fileName || undefined,
  });
  if (reply?.ok !== true) throw new Error(reply?.error || "Rapid Download Manager refused the download");
  cachedAt = 0; // it is certainly running now
}

// ---------------------------------------------------------------- Downloads

// Fired once the browser knows the file's name (after the response headers,
// before anything is saved or a "Save as" dialog is shown). The download
// cannot finish until `suggest` is called, which is what makes the hand-over
// free of races. The listener must be synchronous and return true.
chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  takeOver(item)
    .catch(() => false)
    .then((taken) => {
      if (!taken) suggest();
    });
  return true;
});

async function takeOver(item) {
  if (item.byExtensionId === chrome.runtime.id) return false;
  const url = item.finalUrl || item.url;
  if (!isWeb(url)) return false; // blob:, data:, file: stay in the browser
  if (!(await settings()).enabled) return false;

  const status = await rdmStatus();
  if (!status.installed) return false;
  const fileName = leafName(item.filename);
  const ext = extensionOf(fileName) || extensionOf(url);
  if (!ext || !status.extensions.includes(ext)) return false;

  // Stop the browser's copy from growing while RDM is asked (which may have to start RDM first).
  await quiet(chrome.downloads.pause(item.id));
  try {
    await sendToRdm({ url, referer: item.referrer, fileName, incognito: item.incognito });
  } catch {
    cachedAt = 0;
    await quiet(chrome.downloads.resume(item.id));
    return false;
  }
  // RDM has it: now, and only now, the browser's copy goes away.
  await quiet(chrome.downloads.cancel(item.id));
  await quiet(chrome.downloads.erase({ id: item.id }));
  return true;
}

// ---------------------------------------------------------------- Media seen on a page

const mediaKey = (tabId) => `media:${tabId}`;

// Storage updates are read-modify-write; run them one at a time.
let storageQueue = Promise.resolve();
function queued(task) {
  storageQueue = storageQueue.then(task, task);
  return storageQueue;
}

async function mediaFor(tabId) {
  const key = mediaKey(tabId);
  const stored = await chrome.storage.session.get(key);
  return Array.isArray(stored[key]) ? stored[key] : [];
}

function rememberMedia(tabId, entry) {
  return queued(async () => {
    const list = await mediaFor(tabId);
    const known = list.find((m) => m.url === entry.url);
    if (known) {
      if (entry.size && !known.size) known.size = entry.size;
    } else {
      list.push(entry);
      while (list.length > MAX_MEDIA_PER_TAB) list.shift();
    }
    await chrome.storage.session.set({ [mediaKey(tabId)]: list });
    await showCount(tabId, (await offersFor(tabId)).length);
  });
}

function forgetMedia(tabId) {
  return queued(async () => {
    await chrome.storage.session.remove([mediaKey(tabId), fbKey(tabId)]);
    await showCount(tabId, 0);
  });
}

async function showCount(tabId, count) {
  await quiet(chrome.action.setBadgeBackgroundColor({ tabId, color: "#1565c0" }));
  await quiet(chrome.action.setBadgeText({ tabId, text: count > 0 ? String(count) : "" }));
}

// Whole files first (largest first), then streams in the order they appeared
// (a player asks for the master playlist before the per-quality ones).
function ranked(list) {
  const rank = (m) => (m.kind === "video" ? 0 : m.kind === "stream" ? 1 : 2);
  return [...list].sort((a, b) => rank(a) - rank(b) || (a.kind === "stream" ? a.seen - b.seen : (b.size || 0) - (a.size || 0)));
}

function classify(details) {
  if (details.tabId < 0 || (details.statusCode !== 200 && details.statusCode !== 206)) return null;
  if (EXCLUDED_HOSTS.test(hostOf(details.url)) || EXCLUDED_HOSTS.test(hostOf(details.initiator || ""))) return null;

  const headers = {};
  for (const h of details.responseHeaders || []) headers[h.name.toLowerCase()] = h.value || "";
  const type = (headers["content-type"] || "").split(";")[0].trim().toLowerCase();
  const ext = extensionOf(details.url);

  if (PLAYLIST_TYPES.has(type) || ext === "m3u8") {
    return { url: details.url, kind: "stream", type, size: 0, seen: Date.now() };
  }
  // Only what a <video>/<audio> element fetched itself: script-driven requests
  // of these types are the fragments of an adaptive stream, not files.
  if (details.type !== "media") return null;
  if (SEGMENT_EXTENSIONS.has(ext) || type === "video/mp2t") return null;
  const isAudio = type.startsWith("audio/");
  if (!isAudio && !type.startsWith("video/") && !MEDIA_EXTENSIONS.has(ext)) return null;

  // A ranged answer carries the full size after the slash: "bytes 0-999/123456".
  const total = /\/(\d+)\s*$/.exec(headers["content-range"] || "");
  const size = total ? Number(total[1]) : details.statusCode === 200 ? Number(headers["content-length"]) || 0 : 0;
  if (size && size < MIN_MEDIA_BYTES) return null; // interface sounds, tiny loops
  return { url: details.url, kind: isAudio ? "audio" : "video", type, size, seen: Date.now() };
}

const WEB = ["http://*/*", "https://*/*"];

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (details.tabId < 0) return;
    if (noteFacebookTrack(details)) return;
    const entry = classify(details);
    if (entry) rememberMedia(details.tabId, entry);
  },
  { urls: WEB, types: ["media", "xmlhttprequest", "object", "other"] },
  ["responseHeaders"],
);

// A new page in the tab: what the previous one played is no longer on offer.
chrome.webRequest.onBeforeRequest.addListener(
  (details) => {
    if (details.tabId >= 0) forgetMedia(details.tabId);
  },
  { urls: WEB, types: ["main_frame"] },
);

chrome.tabs.onRemoved.addListener((tabId) => {
  queued(() => chrome.storage.session.remove([mediaKey(tabId), fbKey(tabId)]));
});

// ---------------------------------------------------------------- Facebook

// Facebook's player never gives its <video> an address. It fetches the picture
// and the sound as two separate streams, in pieces, from script. Two things
// make a proper download possible all the same:
//
//  * every address on Facebook's video servers describes itself in an "efg"
//    parameter (base64 JSON): which video it belongs to, how long that is,
//    and which encoding this is. So the pieces being fetched tell us which
//    video is playing in the tab;
//  * the data Facebook loads for the page names, for each video, the address
//    of a whole file with picture and sound, in HD and SD. page-hook.js reads
//    those and hands them over ("learn" below).
//
// Per tab: { <video number>: { duration, seen, played, files: { hd, sd }, tracks: { video, audio } } }

const FB_SERVERS = /(^|\.)fbcdn\.net$/i;
const MAX_FB_VIDEOS_PER_TAB = 40;
const fbKey = (tabId) => `fb:${tabId}`;

function facebookInfo(url) {
  if (typeof url !== "string" || url.length > 4096) return null;
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== "https:" || !FB_SERVERS.test(parsed.hostname)) return null;
  const efg = parsed.searchParams.get("efg");
  if (!efg) return null;
  let info;
  try {
    info = JSON.parse(atob(efg.replace(/-/g, "+").replace(/_/g, "/")));
  } catch {
    return null;
  }
  const video = String(info?.xpv_asset_id ?? "");
  if (!/^\d{5,20}$/.test(video)) return null;
  const tag = typeof info.vencode_tag === "string" ? info.vencode_tag : "";
  // "…dash_vp9-basic-gen2_720p", "…C3.224.sve_sd", "…dash_ln_heaac_vbr3_audio"
  const height = Number((/_(\d{3,4})p\b/.exec(tag) || /\.C\d\.(\d{3,4})\./.exec(tag) || [])[1]) || 0;
  return { video, duration: Number(info.duration_s) || 0, audio: /audio/i.test(tag), height };
}

async function fbFor(tabId) {
  const key = fbKey(tabId);
  const stored = await chrome.storage.session.get(key);
  return stored[key] && typeof stored[key] === "object" ? stored[key] : {};
}

function updateFb(tabId, change) {
  return queued(async () => {
    const videos = await fbFor(tabId);
    change((info) => (videos[info.video] ||= { duration: info.duration, seen: Date.now(), played: 0, files: {}, tracks: {} }));
    const ids = Object.keys(videos);
    if (ids.length > MAX_FB_VIDEOS_PER_TAB) {
      const age = (id) => Math.max(videos[id].played, videos[id].seen);
      ids.sort((a, b) => age(a) - age(b));
      for (const id of ids.slice(0, ids.length - MAX_FB_VIDEOS_PER_TAB)) delete videos[id];
    }
    await chrome.storage.session.set({ [fbKey(tabId)]: videos });
    await showCount(tabId, (await offersFor(tabId)).length);
  });
}

// A player fetches a piece every second or so; once every few seconds is enough to know what is playing.
const fbNoted = new Map();

function noteFacebookTrack(details) {
  const info = facebookInfo(details.url);
  if (!info) return false;
  const key = `${details.tabId}:${info.video}:${info.audio ? "audio" : info.height}`;
  if (Date.now() - (fbNoted.get(key) || 0) < 4000) return true;
  if (fbNoted.size > 500) fbNoted.clear();
  fbNoted.set(key, Date.now());
  // Without the two parameters that select a piece, the address is the whole stream.
  const url = details.url.replace(/&(bytestart|byteend)=\d+/g, "");
  updateFb(details.tabId, (entryFor) => {
    const entry = entryFor(info);
    entry.played = Date.now();
    if (info.audio) entry.tracks.audio = { url };
    else if (!entry.tracks.video || info.height >= entry.tracks.video.height) entry.tracks.video = { url, height: info.height };
  });
  return true;
}

function facebookOffers(entry) {
  // "HD video · 720p · 2:16": the length tells videos apart when several are listed.
  const length = entry.duration > 0 ? `${Math.floor(entry.duration / 60)}:${String(Math.floor(entry.duration % 60)).padStart(2, "0")}` : "";
  const label = (what, file) => [what, file.height ? `${file.height}p` : "", length].filter(Boolean).join(" · ");
  const offer = (what, file, kind = "video") => ({ url: file.url, kind, size: 0, label: label(what, file), fb: true });
  const offers = [];
  if (entry.files.hd) offers.push(offer("HD video", entry.files.hd));
  if (entry.files.sd) offers.push(offer("SD video", entry.files.sd));
  if (offers.length) return offers;
  // The whole file is not known (the page's data was not seen): the separate streams are still better than nothing.
  if (entry.tracks.video) offers.push(offer("Video without sound", entry.tracks.video));
  if (entry.tracks.audio) offers.push(offer("Sound only", entry.tracks.audio, "audio"));
  return offers;
}

// Everything on offer in a tab, best first. `duration` (seconds) is the length
// of the video the pointer is on, when known: a feed holds many videos, and
// the player loads the next ones ahead of time, so "the one fetched last" is
// not "the one being watched". Facebook states lengths in whole seconds.
async function offersFor(tabId, duration = 0) {
  const general = ranked(await mediaFor(tabId));
  const videos = Object.values(await fbFor(tabId)).sort((a, b) => b.played - a.played || b.seen - a.seen);
  if (videos.length === 0) return general;
  let chosen;
  if (duration > 0) {
    const distance = (v) => Math.abs(v.duration - Math.floor(duration));
    const nearest = videos.filter((v) => v.duration > 0).sort((a, b) => distance(a) - distance(b))[0];
    chosen = nearest && distance(nearest) <= 1 ? [nearest] : videos.filter((v) => v.played).slice(0, 1);
  } else {
    chosen = videos.filter((v) => v.played).slice(0, 3);
  }
  return [...chosen.flatMap(facebookOffers), ...general];
}

// "(3) Some video | Facebook" → "Some video"
function titleForFile(title) {
  const clean = String(title || "")
    .replace(/^\(\d+\+?\)\s*/, "")
    .replace(/\s*[|\-–]\s*Facebook\s*$/i, "")
    .replace(/[<>:"/\\|?*\u0000-\u001f]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return /^(facebook|watch|video|reels?)?$/i.test(clean) ? "" : clean.slice(0, 80).trim();
}

// A readable name for a Facebook file, whose own name is a long random token.
async function facebookFileName(url, tab) {
  const info = facebookInfo(url);
  if (!info || !tab) return undefined;
  const entry = (await fbFor(tab.id))[info.video];
  const which = entry && Object.keys(entry.files).find((q) => entry.files[q].url === url);
  const detail = which ? which.toUpperCase() : info.audio ? "sound" : `video only${info.height ? ` ${info.height}p` : ""}`;
  return `${titleForFile(tab.title) || "Facebook video"} (${detail}).${info.audio ? "m4a" : "mp4"}`;
}

// ---------------------------------------------------------------- Requests from the page button and the popup

// The last hand-over per tab: a second click on the same thing is not a second download.
const lastRequest = new Map();

const handlers = {
  // The media on offer in a tab.
  async media(message, sender) {
    const tabId = sender.tab ? sender.tab.id : message.tabId;
    if (!Number.isInteger(tabId)) return { ok: true, media: [] };
    const duration = Number.isFinite(message.duration) && message.duration > 0 ? message.duration : 0;
    const offers = await offersFor(tabId, duration);
    // The page button belongs to one video. If that video's length is not known
    // yet and the tab holds several, wait rather than offer a guess.
    if (sender.tab && duration === 0 && Object.keys(await fbFor(tabId)).length > 1) {
      return { ok: true, media: offers.filter((offer) => !offer.fb), pending: true };
    }
    return { ok: true, media: offers };
  },

  // Addresses of whole video files that page-hook.js found in the page's data.
  async learn(message, sender) {
    if (!sender.tab || !Array.isArray(message.items)) return { ok: false };
    const found = [];
    for (const item of message.items.slice(0, 80)) {
      const info = item && facebookInfo(item.url); // checks the address is Facebook's own, whatever the page said
      if (info && !info.audio) found.push({ info, url: item.url, quality: item.quality === "hd" ? "hd" : "sd" });
    }
    if (found.length) {
      await updateFb(sender.tab.id, (entryFor) => {
        for (const { info, url, quality } of found) entryFor(info).files[quality] = { url, height: info.height };
      });
    }
    return { ok: true };
  },

  async download(message, sender) {
    const url = message.url;
    if (!isWeb(url) || url.length > 8192) throw new Error("Not a web address");
    if (EXCLUDED_HOSTS.test(hostOf(url))) throw new Error("Downloads from this site are not supported");

    // A content script only ever speaks for its own tab; the popup names the tab it was opened on.
    const tab = sender.tab || (Number.isInteger(message.tabId) ? await quiet(chrome.tabs.get(message.tabId)) : null);
    const slot = tab ? tab.id : "popup";
    const last = lastRequest.get(slot);
    if (last && Date.now() - last.at < 3000) {
      if (last.url === url) return { ok: true }; // already on its way
      if (Date.now() - last.at < 400) throw new Error("Too many requests at once; try again");
    }
    lastRequest.set(slot, { url, at: Date.now() });

    // The referrer is what the browser says the frame is, never what the page claims.
    const referer = sender.tab ? sender.url || sender.tab.url : tab && tab.url;
    try {
      await sendToRdm({ url, referer, fileName: await facebookFileName(url, tab), incognito: tab ? tab.incognito : false });
    } catch (error) {
      lastRequest.delete(slot); // it did not go out: the next click may try again
      throw error;
    }
    return { ok: true };
  },

  // For the popup.
  async status() {
    const status = await rdmStatus(0);
    return { ok: true, status: { installed: status.installed, running: status.running, version: status.version }, settings: await settings() };
  },
};

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  // Only this extension's own pages and content scripts.
  if (sender.id !== chrome.runtime.id || !message || typeof message.type !== "string") return false;
  if (!Object.hasOwn(handlers, message.type)) return false;
  handlers[message.type](message, sender).then(sendResponse, (error) => sendResponse({ ok: false, error: String(error?.message || error) }));
  return true;
});

// ---------------------------------------------------------------- Context menu

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "rdm-download-link",
      title: "Download with RDM",
      contexts: ["link", "video", "audio", "image"],
    });
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId !== "rdm-download-link") return;
  let url = info.linkUrl || info.srcUrl;
  // A player fed by script has no address of its own; offer what the page actually loaded.
  if (!isWeb(url) && tab && (info.mediaType === "video" || info.mediaType === "audio")) {
    url = (await offersFor(tab.id))[0]?.url;
  }
  if (!isWeb(url) || EXCLUDED_HOSTS.test(hostOf(url))) return;
  // On failure there is nothing useful to do from here; the popup shows whether RDM is reachable.
  await quiet(sendToRdm({ url, referer: info.frameUrl || info.pageUrl, incognito: tab ? tab.incognito : false }));
});
