// Rapid Download Manager — reading video addresses out of a site's own data.
//
// Runs in the PAGE's world (not the extension's), and only on the sites named
// in manifest.json. Some sites never give their <video> element an address:
// the player is fed piece by piece from script, and the address of the whole
// file (picture and sound together) exists only inside the data the site
// loads for itself. This script looks at that data as it arrives and passes
// the addresses on.
//
// It has none of the extension's privileges, changes nothing the site does,
// and sends nothing anywhere: what it finds goes to the extension's content
// script in the same tab, which checks every address again.

(() => {
  "use strict";

  // Facebook: "browser_native_hd_url":"https:\/\/video…fbcdn.net\/…mp4?…"
  const PATTERN = /"browser_native_(hd|sd)_url":"((?:[^"\\]|\\.)*)"/g;
  const MARKER = "browser_native_";
  const reported = new Set();

  function report(text) {
    if (typeof text !== "string" || !text.includes(MARKER)) return;
    const items = [];
    for (const match of text.matchAll(PATTERN)) {
      let url;
      try {
        url = JSON.parse(`"${match[2]}"`);
      } catch {
        continue;
      }
      if (reported.has(url)) continue;
      if (reported.size > 4000) reported.clear();
      reported.add(url);
      items.push({ url, quality: match[1] });
    }
    if (items.length) window.postMessage({ rdm: "learned-media", items }, location.origin);
  }

  // Never let a mistake in here disturb the site.
  const quietly = (task) => {
    try {
      task();
    } catch {
      // Ignored on purpose.
    }
  };

  // Data loaded while browsing arrives through XMLHttpRequest or fetch.
  const nativeSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function send(...args) {
    quietly(() => {
      this.addEventListener("load", () => {
        quietly(() => {
          if (this.responseType === "" || this.responseType === "text") report(this.responseText);
        });
      });
    });
    return nativeSend.apply(this, args);
  };

  const nativeFetch = window.fetch;
  if (typeof nativeFetch === "function") {
    window.fetch = function fetch(...args) {
      const pending = nativeFetch.apply(this, args);
      quietly(() => {
        pending.then(
          (response) => {
            quietly(() => {
              const type = response.headers.get("content-type") || "";
              // The site's own data, not media or images.
              if (response.url.startsWith(location.origin) && !/^(video|audio|image|font)\//i.test(type)) {
                response.clone().text().then(report, () => {});
              }
            });
          },
          () => {},
        );
      });
      return pending;
    };
  }

  // The data for the page one arrives on is embedded in the document itself.
  function readEmbedded() {
    for (const script of document.querySelectorAll('script[type="application/json"]')) {
      quietly(() => report(script.textContent));
    }
  }
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", readEmbedded, { once: true });
  else readEmbedded();
})();
