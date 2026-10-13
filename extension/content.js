// Rapid Download Manager — the "Download with RDM" button on videos.
//
// Runs in every frame, in the extension's isolated world, from the very start
// of the page. The page is not trusted:
//
//  * nothing it controls is ever interpreted as markup or code;
//  * the button lives in a closed shadow tree with its own styles, in the
//    browser's top layer so that nothing the page draws can cover it;
//  * only real clicks do anything, never ones a page script synthesises;
//  * clicks on the button are taken away before the page can see them. Video
//    sites hang "open an advert in a new tab" on every click anywhere on the
//    page; without this the first click on the button would go to them.
//
// All this script can ask of the background worker is "offer this address to
// RDM", and RDM shows the user its own dialog before any download starts.

(() => {
  "use strict";

  // Sites whose terms forbid downloading; the button never appears there.
  const EXCLUDED_HOSTS = /(^|\.)(youtube\.com|youtu\.be|youtube-nocookie\.com|googlevideo\.com)$/i;
  if (EXCLUDED_HOSTS.test(location.hostname)) return;

  const MIN_WIDTH = 200;
  const MIN_HEIGHT = 120;
  const LINGER_MS = 2500;
  const LABEL = "Download with RDM";

  let wanted = true; // the "show the button on videos" setting
  let dismissed = false; // closed with × on this page
  let alive = true; // false once the extension was reloaded under us
  let video = null; // the element the button belongs to right now
  let offers = []; // what could be downloaded for it, best first
  let ui = null;
  let hideTimer = 0;
  let resetTimer = 0;
  let sending = false;

  const isWeb = (url) => typeof url === "string" && /^https?:\/\//i.test(url);

  function hostOf(url) {
    try {
      return new URL(url).hostname;
    } catch {
      return "";
    }
  }

  function nameOf(url) {
    try {
      const leaf = new URL(url).pathname.split("/").filter(Boolean).pop() || "";
      return decodeURIComponent(leaf) || hostOf(url);
    } catch {
      return url;
    }
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

  // Ask the background worker; `null` when it cannot be reached.
  async function ask(message) {
    if (!alive) return null;
    try {
      return await chrome.runtime.sendMessage(message);
    } catch {
      // The extension was updated or removed: this copy of the script is orphaned.
      if (!chrome.runtime?.id) shutDown();
      return null;
    }
  }

  // ---- Clicks on the button never reach the page

  // Registered before any script of the page runs, on the outermost target and
  // in the capture phase: for these events, this listener is the first to run.
  const SHIELDED = ["click", "dblclick", "auxclick", "contextmenu", "mousedown", "mouseup", "pointerdown", "pointerup", "touchstart", "touchend"];

  function shield(event) {
    // Events from inside the closed shadow tree appear, out here, to come from its host.
    if (!ui || event.target !== ui.host) return;
    event.stopImmediatePropagation();
    if (event.type === "click") {
      event.preventDefault();
      if (event.isTrusted) activate(event); // not a click a page script made up
    } else if (event.type === "dblclick" || event.type === "auxclick" || event.type === "contextmenu") {
      event.preventDefault();
    }
  }

  // Which part of the button a click landed on. The event itself was stopped
  // above, so it is looked up by position (or by focus, for the keyboard).
  function activate(event) {
    const hit = event.detail === 0 ? ui.shadow.activeElement : ui.shadow.elementFromPoint(event.clientX, event.clientY);
    const control = hit && typeof hit.closest === "function" ? hit.closest("[data-act]") : null;
    if (!control) return;
    switch (control.dataset.act) {
      case "send":
        ui.menu.hidden = true;
        send(offers[0]);
        break;
      case "more":
        toggleMenu();
        break;
      case "pick":
        ui.menu.hidden = true;
        send(offers[Number(control.dataset.index)]);
        break;
      case "close":
        dismissed = true;
        hide();
        break;
    }
  }

  // ---- What can be downloaded for a video element

  function ownSources(element) {
    const urls = [element.currentSrc, element.src];
    for (const source of element.querySelectorAll("source[src]")) urls.push(source.src);
    return [...new Set(urls)]
      .filter((url) => isWeb(url) && !EXCLUDED_HOSTS.test(hostOf(url)))
      .map((url) => ({ url, kind: /\.m3u8($|\?)/i.test(url) ? "stream" : "video", size: 0 }));
  }

  // The element's own address first, then what the background worker saw the
  // page load. The element's length helps it pick the right video on pages
  // that show several.
  async function findOffers(element) {
    const reply = await ask({ type: "media", duration: Number.isFinite(element.duration) ? element.duration : 0 });
    // "Not sure yet which video this is": look again shortly, the player is still starting.
    if (reply?.pending && (retries.get(element) || 0) < 6) {
      retries.set(element, (retries.get(element) || 0) + 1);
      setTimeout(() => element.isConnected && showFor(element), 1200);
    }
    const seen = Array.isArray(reply?.media) ? reply.media.filter((m) => m && isWeb(m.url)) : [];
    const known = new Set();
    const merged = [];
    for (const offer of [...ownSources(element), ...seen]) {
      if (known.has(offer.url)) continue;
      known.add(offer.url);
      const detail = seen.find((m) => m.url === offer.url);
      merged.push({ url: offer.url, kind: detail?.kind || offer.kind, size: detail?.size || 0, label: detail?.label || "" });
    }
    return merged.slice(0, 8);
  }

  // ---- The button

  const STYLE = `
    :host { all: initial; }
    .wrap { position: relative; font: 600 12px/1 "Segoe UI", system-ui, sans-serif; color: #fff; user-select: none; }
    .bar { display: flex; align-items: stretch; border-radius: 6px; overflow: hidden; box-shadow: 0 2px 10px rgba(0, 0, 0, .45); }
    button { all: unset; cursor: pointer; display: flex; align-items: center; background: #1565c0; color: #fff; }
    button:hover { background: #1976d2; }
    button:focus-visible { outline: 2px solid #fff; outline-offset: -2px; }
    button[hidden] { display: none; }
    .main { gap: 7px; padding: 8px 11px 8px 9px; white-space: nowrap; }
    .main svg { width: 14px; height: 14px; flex: none; }
    .more, .close { border-left: 1px solid rgba(255, 255, 255, .25); }
    .more { padding: 0 8px; }
    .more::before { content: ""; border: 4px solid transparent; border-top-color: #fff; margin-top: 4px; }
    .close { padding: 0 9px; font-size: 15px; font-weight: 400; }
    .wrap.busy button.main { background: #455a64; cursor: progress; }
    .wrap.done button.main { background: #2e7d32; }
    .wrap.failed button.main { background: #b3261e; }
    .menu { position: absolute; top: calc(100% + 4px); right: 0; min-width: 260px; max-width: 380px; max-height: 260px; overflow: auto;
            background: #fff; color: #1f1f1f; border-radius: 6px; box-shadow: 0 4px 18px rgba(0, 0, 0, .45); padding: 4px; }
    .menu[hidden] { display: none; }
    .menu button { background: none; color: inherit; width: 100%; box-sizing: border-box; padding: 7px 9px; border-radius: 4px;
                   flex-direction: column; align-items: flex-start; gap: 4px; font-weight: 400; }
    .menu button:hover { background: #e8f0fe; }
    .name { font-weight: 600; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; pointer-events: none; }
    .meta { font-size: 11px; color: #5f6368; pointer-events: none; }
  `;

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text) node.textContent = text;
    return node;
  }

  function control(className, act, text) {
    const node = element("button", className, text);
    node.type = "button";
    node.dataset.act = act;
    return node;
  }

  function arrowIcon() {
    const ns = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(ns, "svg");
    svg.setAttribute("viewBox", "0 0 16 16");
    const path = document.createElementNS(ns, "path");
    path.setAttribute("d", "M8 1v8.2M4.4 6.2 8 9.8l3.6-3.6M2.5 13.5h11");
    path.setAttribute("fill", "none");
    path.setAttribute("stroke", "currentColor");
    path.setAttribute("stroke-width", "1.8");
    path.setAttribute("stroke-linecap", "round");
    path.setAttribute("stroke-linejoin", "round");
    svg.append(path);
    return svg;
  }

  function buildUi() {
    const host = document.createElement("div");
    // Inline styles set through the object model are not subject to the page's style policy.
    for (const [name, value] of [["all", "initial"], ["position", "fixed"], ["z-index", "2147483647"], ["display", "none"]]) {
      host.style.setProperty(name, value, "important");
    }
    // The top layer is above every z-index, so an overlay the page spreads
    // over its player cannot sit between the pointer and the button.
    const topLayer = typeof host.showPopover === "function";
    if (topLayer) host.setAttribute("popover", "manual");

    const shadow = host.attachShadow({ mode: "closed" });
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(STYLE);
    shadow.adoptedStyleSheets = [sheet];

    const wrap = element("div", "wrap");
    const bar = element("div", "bar");
    const main = control("main", "send");
    const label = element("span", "", LABEL);
    main.append(arrowIcon(), label);
    const more = control("more", "more");
    more.title = "Choose another version";
    const close = control("close", "close", "×");
    close.title = "Hide on this page";
    const menu = element("div", "menu");
    menu.hidden = true;
    bar.append(main, more, close);
    wrap.append(bar, menu);
    shadow.append(wrap);

    host.addEventListener("mouseenter", () => clearTimeout(hideTimer));
    host.addEventListener("mouseleave", () => scheduleHide());
    return { host, shadow, wrap, main, label, more, menu, topLayer };
  }

  function describe(offer) {
    const kind = offer.kind === "stream" ? "Stream (HLS)" : offer.kind === "audio" ? "Audio" : "Video";
    return { name: offer.label || nameOf(offer.url), meta: [offer.label ? "" : kind, formatSize(offer.size), hostOf(offer.url)].filter(Boolean).join(" · ") };
  }

  function toggleMenu() {
    const { menu } = ui;
    if (!menu.hidden) {
      menu.hidden = true;
      return;
    }
    menu.replaceChildren();
    offers.forEach((offer, index) => {
      const item = control("", "pick");
      item.dataset.index = String(index);
      item.title = offer.url;
      const { name, meta } = describe(offer);
      item.append(element("span", "name", name), element("span", "meta", meta));
      menu.append(item);
    });
    menu.hidden = false;
  }

  function setState(state, text, hint) {
    clearTimeout(resetTimer);
    ui.wrap.classList.remove("busy", "done", "failed");
    if (state) ui.wrap.classList.add(state);
    ui.label.textContent = text;
    ui.main.title = hint || "";
  }

  async function send(offer) {
    if (!offer || sending) return;
    sending = true;
    setState("busy", "Sending…"); // starting RDM can take a moment; show that the click arrived
    const reply = await ask({ type: "download", url: offer.url });
    sending = false;
    if (!ui) return;
    if (reply?.ok === true) setState("done", "Sent to RDM");
    else setState("failed", "Could not send", reply?.error || "Rapid Download Manager could not be reached");
    resetTimer = setTimeout(() => ui && setState("", LABEL, describe(offers[0] || offer).name), 2500);
  }

  function place() {
    if (!ui || !video) return false;
    const rect = video.getBoundingClientRect();
    const visible = rect.width >= MIN_WIDTH && rect.height >= MIN_HEIGHT && rect.bottom > 40 && rect.top < innerHeight - 40 && rect.right > 80 && rect.left < innerWidth - 80;
    if (!visible || !video.isConnected) return false;
    const width = ui.host.offsetWidth || 190;
    const top = Math.max(rect.top + 10, 6);
    const left = Math.min(Math.max(rect.right - width - 10, 6), innerWidth - width - 6);
    ui.host.style.setProperty("top", `${Math.round(top)}px`, "important");
    ui.host.style.setProperty("left", `${Math.round(left)}px`, "important");
    return true;
  }

  const shown = () => Boolean(ui) && ui.host.style.getPropertyValue("display") === "block";

  let lookingFor = null;
  let shownLength = 0; // length of the video the current offers were found for
  const lengthOf = (element) => (Number.isFinite(element.duration) ? Math.floor(element.duration) : 0);
  const retries = new WeakMap(); // video → how often the worker said "not sure yet"
  const nothingAt = new WeakMap(); // video → when a lookup last came back empty
  async function showFor(target) {
    if (!alive || !wanted || dismissed || document.fullscreenElement) return;
    clearTimeout(hideTimer);
    // Already showing for this element — unless it now plays something else (feeds reuse their players).
    // (A length that is momentarily unknown, as when a clip loops, is not "something else".)
    const sameVideo = shownLength === lengthOf(target) || lengthOf(target) === 0;
    if ((video === target && shown() && sameVideo) || lookingFor === target) return;
    if (Date.now() - (nothingAt.get(target) || 0) < 1000) return; // do not ask on every pointer move
    lookingFor = target;
    const found = await findOffers(target);
    lookingFor = null;
    if (found.length === 0) nothingAt.set(target, Date.now());
    if (!alive) return;
    if (found.length === 0) {
      if (video === target) hide(); // what it showed belongs to the previous video
      return;
    }
    video = target;
    offers = found;
    shownLength = lengthOf(target);
    if (!ui) ui = buildUi();
    if (!ui.host.isConnected) document.documentElement.append(ui.host);
    ui.menu.hidden = true;
    ui.more.hidden = offers.length < 2;
    if (!sending) setState("", LABEL, describe(offers[0]).name);
    ui.host.style.setProperty("display", "block", "important");
    if (ui.topLayer) {
      try {
        if (!ui.host.matches(":popover-open")) ui.host.showPopover();
      } catch {
        // Not in the top layer then; the z-index still applies.
      }
    }
    if (!place()) hide();
  }

  function hide() {
    clearTimeout(hideTimer);
    if (ui) {
      ui.menu.hidden = true;
      if (ui.topLayer) {
        try {
          ui.host.hidePopover();
        } catch {
          // Was not open.
        }
      }
      ui.host.style.setProperty("display", "none", "important");
    }
    video = null;
  }

  function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(hide, LINGER_MS);
  }

  // ---- When to show it

  function bigEnough(candidate) {
    const rect = candidate.getBoundingClientRect();
    return rect.width >= MIN_WIDTH && rect.height >= MIN_HEIGHT;
  }

  // The video under the pointer. Players cover the video with their own
  // controls, and some switch off hit-testing on the video itself, so after
  // looking through what is under the pointer, compare positions directly.
  function videoAt(x, y) {
    for (const node of document.elementsFromPoint(x, y)) {
      if (node instanceof HTMLVideoElement && bigEnough(node)) return node;
    }
    for (const node of document.querySelectorAll("video")) {
      const rect = node.getBoundingClientRect();
      if (x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom && bigEnough(node)) return node;
    }
    return null;
  }

  // A video started playing (media events do not bubble, but they can be captured).
  function onPlaying(event) {
    const target = event.target;
    if (target instanceof HTMLVideoElement && bigEnough(target)) {
      showFor(target).then(() => scheduleHide());
    }
  }

  let lastMove = 0;
  function onMove(event) {
    if (Date.now() - lastMove < 150) return;
    lastMove = Date.now();
    if (ui && event.target === ui.host) return;
    const under = videoAt(event.clientX, event.clientY);
    if (under) showFor(under);
    else if (video) scheduleHide();
  }

  function onViewChange() {
    if (video && !place()) hide();
  }

  function onSettingChange(changes, area) {
    if (area !== "local" || !changes.videoButton) return;
    wanted = changes.videoButton.newValue !== false;
    if (!wanted) hide();
  }

  // ---- Sites that feed their player from script

  // On those sites a small script in the page's own world (page-hook.js) reads
  // the addresses of whole video files out of the data the site loads, and
  // posts them here. The page could post such a message too, so each address
  // is checked here and again by the background worker; all it can ever
  // become is one more entry offered under the button.
  const LEARNS_FROM_PAGE = /(^|\.)facebook\.com$/i.test(location.hostname);

  function onPageMessage(event) {
    if (event.source !== window || event.origin !== location.origin) return;
    const data = event.data;
    if (!data || data.rdm !== "learned-media" || !Array.isArray(data.items)) return;
    const items = data.items
      .slice(0, 80)
      .filter((item) => item && isWeb(item.url) && item.url.length < 4096)
      .map((item) => ({ url: item.url, quality: item.quality === "hd" ? "hd" : "sd" }));
    if (items.length) ask({ type: "learn", items });
  }

  function shutDown() {
    alive = false;
    for (const type of SHIELDED) removeEventListener(type, shield, true);
    document.removeEventListener("playing", onPlaying, true);
    document.removeEventListener("mousemove", onMove, true);
    removeEventListener("scroll", onViewChange, true);
    removeEventListener("resize", onViewChange);
    removeEventListener("message", onPageMessage);
    document.removeEventListener("fullscreenchange", hide);
    if (ui) ui.host.remove();
    ui = null;
  }

  for (const type of SHIELDED) addEventListener(type, shield, true);
  document.addEventListener("playing", onPlaying, true);
  document.addEventListener("mousemove", onMove, { capture: true, passive: true });
  addEventListener("scroll", onViewChange, { capture: true, passive: true });
  addEventListener("resize", onViewChange, { passive: true });
  document.addEventListener("fullscreenchange", hide);
  if (LEARNS_FROM_PAGE) addEventListener("message", onPageMessage);

  try {
    chrome.storage.local.get({ videoButton: true }).then((stored) => {
      wanted = stored.videoButton !== false;
    });
    chrome.storage.onChanged.addListener(onSettingChange);
  } catch {
    shutDown();
  }
})();
