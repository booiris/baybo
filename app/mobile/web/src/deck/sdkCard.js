// Card-side SDK — inlined by the shell into every card iframe's srcdoc
// BEFORE the agent-written fragment, so `deck` exists synchronously for
// card inline scripts. The card runs in an opaque origin with a CSP
// that blocks all network; its only I/O is the MessagePort the shell
// transfers at init. Card code never touches the handshake.
(function () {
  "use strict";
  let port = null;
  let initialized = false;
  let size = "wide";
  let latest;
  let hasData = false;
  const dataCbs = [];
  const sizeCbs = [];
  const queuedCalls = [];
  const pending = new Map();
  let nextId = 1;

  function applySize(nextSize) {
    size = nextSize;
    document.documentElement.dataset.deckSize = nextSize;
  }

  applySize(size);

  function notifySize() {
    for (const cb of sizeCbs) {
      try {
        cb(size);
      } catch (err) {
        if (port)
          port.postMessage({ type: "log", level: "error", message: String(err) });
      }
    }
    scheduleReachCheck();
  }

  // A `max` layout taller than the viewport that nothing can scroll strands
  // its own content: the page looks cut off and no gesture reaches the rest.
  // Nothing upstream can catch this — the install gate never renders
  // `card.html` — and every cause presents identically here (CSS that clips
  // or fixes the box, a JS-set inline style, an absolutely positioned child),
  // so this measures the rendered result instead of predicting it. Reported
  // through the existing log channel; a card re-rendering every tick reports
  // only when the shortfall changes.
  let lastStranded = 0;
  function checkReachable() {
    if (size !== "max" || !port) return;
    const root = document.scrollingElement || document.documentElement;
    const stranded = document.body.scrollHeight - window.innerHeight;
    if (stranded > 4 && root.scrollHeight - root.clientHeight <= 1) {
      if (stranded === lastStranded) return;
      lastStranded = stranded;
      port.postMessage({
        type: "log",
        level: "error",
        message:
          "max layout: " +
          stranded +
          "px of content sits below the viewport and nothing scrolls to it — " +
          "the card's own CSS is overriding the injected scroll container " +
          "(do not set height/overflow/position on .card in a max rule)",
      });
    } else if (stranded <= 4) {
      lastStranded = 0;
    }
  }

  // Two frames: the card renders synchronously off onSizeChange/onData, and
  // layout must settle before the measurement means anything.
  function scheduleReachCheck() {
    requestAnimationFrame(function () {
      requestAnimationFrame(checkReachable);
    });
  }

  // Coerce `pickBlob({accept})` to plain strings ONLY so the port's structured
  // clone can't throw on an exotic value. The shell is the single normalizer —
  // the token grammar and its validation live there, not in two places.
  function acceptOf(opts) {
    const raw = opts && opts.accept;
    if (Array.isArray(raw)) return raw.map(String);
    return raw == null ? null : String(raw);
  }

  function flushQueued() {
    while (queuedCalls.length > 0) {
      port.postMessage(queuedCalls.shift());
    }
  }

  function onPortMessage(e) {
    const msg = e.data || {};
    if (msg.type === "data") {
      latest = msg.payload;
      hasData = true;
      for (const cb of dataCbs) {
        try {
          cb(latest);
        } catch (err) {
          port.postMessage({ type: "log", level: "error", message: String(err) });
        }
      }
      scheduleReachCheck();
    } else if (msg.type === "call_result") {
      const p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.value);
      else p.reject(new Error(msg.error || "call failed"));
    } else if (msg.type === "pick_result") {
      const p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.ref);
      else p.reject(new Error(msg.error || "pick failed"));
    } else if (msg.type === "size") {
      if (msg.size && msg.size !== size) {
        applySize(msg.size);
        notifySize();
      }
    }
  }

  window.addEventListener("message", function (e) {
    // The shell is this iframe's direct parent; a co-resident card is not.
    // Pinning the port to the FIRST `deck_init` that came from `window.parent`
    // stops a sibling card from postMessage-ing a forged init with its own
    // port to hijack this card's channel (read its op params, inject spoofed
    // snapshot/result data). Opaque-origin frames all report origin "null", so
    // identity is the source check, not the origin. The shell sends `deck_init`
    // exactly once per mount, so the once-guard never drops a legitimate init.
    if (
      !initialized &&
      e.source === window.parent &&
      e.data &&
      e.data.type === "deck_init" &&
      e.ports &&
      e.ports[0]
    ) {
      initialized = true;
      port = e.ports[0];
      if (e.data.size) applySize(e.data.size);
      port.onmessage = onPortMessage;
      flushQueued();
      // The card's inline script runs (and registers onSizeChange) BEFORE this
      // init arrives, so those callbacks first fired with the default size.
      // Re-fire with the real initial size or the card renders the wrong
      // layout for its tile (e.g. the `wide` layout in a `large` tile).
      notifySize();
    }
  });

  window.deck = {
    onData: function (cb) {
      dataCbs.push(cb);
      if (hasData) {
        try {
          cb(latest);
        } catch {
          /* card callback error — nothing to do */
        }
      }
    },
    /// Register a size-change callback. Fires immediately with the current
    /// size, then on every change — including `"max"` while maximized — so a
    /// card can drive one render path off `deck.size`. `size` is one of
    /// "small" | "wide" | "large" | "max".
    onSizeChange: function (cb) {
      sizeCbs.push(cb);
      try {
        cb(size);
      } catch {
        /* card callback error — nothing to do */
      }
    },
    call: function (op, params) {
      return new Promise(function (resolve, reject) {
        const id = nextId++;
        pending.set(id, { resolve: resolve, reject: reject });
        const msg = { type: "call", id: id, op: String(op), params: params ?? {} };
        if (port) port.postMessage(msg);
        else queuedCalls.push(msg);
      });
    },
    /// Ask the user to pick a file (docs/modules/deck.md §Blobs). Resolves to
    /// a blob ref `{blobId, contentType, size, name}` once the native picker
    /// uploads it — the bytes never cross into the card; pass `blobId` as a
    /// plain string into a following `deck.call(op, {blobId})` for the service
    /// to consume, and/or `deck.blobUrl(ref)` to display it (images only —
    /// the card CSP admits no other blob rendering). Rejects on cancel
    /// (`"cancelled"`), on another pick already in progress (`"busy"`), or on
    /// an upload failure. `opts.accept` is a mime-glob list — a string
    /// ("image/*", or comma-separated) or an array — and elects the picker:
    /// all-image (or absent) opens the photo library, anything else the file
    /// browser. Every call settles exactly once.
    pickBlob: function (opts) {
      return new Promise(function (resolve, reject) {
        const id = nextId++;
        pending.set(id, { resolve: resolve, reject: reject });
        const msg = { type: "pick", id: id, accept: acceptOf(opts) };
        if (port) port.postMessage(msg);
        else queuedCalls.push(msg);
      });
    },
    /// Emit a diagnostic line to the host log (shell → native NSLog). The
    /// card has no console the operator can reach, so this is its only voice
    /// for debugging. Queued like `call` until the port arrives.
    log: function (message, level) {
      const msg = {
        type: "log",
        level: level === "error" ? "error" : "info",
        message: String(message),
      };
      if (port) port.postMessage(msg);
      else queuedCalls.push(msg);
    },
    /// Build a displayable URL for a blob the service produced/was handed
    /// (docs/modules/deck.md §Blobs). Point an `<img src>` at it — the shell's
    /// request interceptor (allowed by the card CSP's `img-src <host origin>`)
    /// serves the bytes through the native side, cache-first. Accepts a ref
    /// object ({blobId|blob_id, contentType|content_type|mime}) or a bare id
    /// string + optional contentType. The id rides the path RAW and the mime
    /// rides `?ct=` encoded, so the same (id, mime) always yields a
    /// byte-identical URL (WebKit keys its memory cache on the full URL).
    /// A blob that isn't cached and can't be fetched (offline/unbound) fails
    /// the load — handle `<img onerror>` if the card must recover.
    blobUrl: function (ref, contentType) {
      let blobId;
      let ct;
      if (ref && typeof ref === "object") {
        blobId = ref.blobId ?? ref.blob_id;
        ct = ref.contentType ?? ref.content_type ?? ref.mime ?? ref.ct;
      } else {
        blobId = ref;
        ct = contentType;
      }
      if (!blobId) return "";
      const q = ct ? "?ct=" + encodeURIComponent(String(ct)) : "";
      // Root-relative: an `about:srcdoc` frame resolves against its parent
      // document's base URL, so this lands on whichever origin the host serves
      // the bundle from without the card knowing which shell it is in.
      return "/blob/" + String(blobId) + q;
    },
    /// Offer a blob to the user via the native share sheet (save to Photos /
    /// Files, AirDrop, …). Fire-and-forget — native fetches the bytes
    /// (cache-first), materializes them under a real filename, and presents the
    /// sheet. Accepts a ref object or a bare id; `opts.filename` /
    /// `opts.contentType` override the ref's.
    shareBlob: function (ref, opts) {
      opts = opts || {};
      let blobId;
      let ct;
      let name;
      if (ref && typeof ref === "object") {
        blobId = ref.blobId ?? ref.blob_id;
        ct = ref.contentType ?? ref.content_type ?? ref.mime;
        name = ref.name;
      } else {
        blobId = ref;
      }
      if (opts.contentType) ct = opts.contentType;
      if (opts.filename) name = opts.filename;
      if (!blobId) return;
      const msg = {
        type: "share",
        blobId: String(blobId),
        filename: name != null ? String(name) : null,
        contentType: ct != null ? String(ct) : null,
      };
      if (port) port.postMessage(msg);
      else queuedCalls.push(msg);
    },
    get size() {
      return size;
    },
  };

  // Edge-swipe-right-to-exit while maximized: a rightward drag that STARTS at
  // the left edge (the iOS back-gesture idiom) collapses the card — reported
  // to the shell, which runs the restore animation. Requiring the edge start
  // keeps a mid-content horizontal drag from exiting by accident. Touches
  // inside this iframe never reach the shell document, so the gesture must be
  // detected here in the injected SDK, not the shell.
  var EDGE_ZONE = 28;
  let swipeX = 0;
  let swipeY = 0;
  let swiping = false;
  window.addEventListener(
    "touchstart",
    function (e) {
      if (
        size !== "max" ||
        e.touches.length !== 1 ||
        e.touches[0].clientX > EDGE_ZONE
      ) {
        swiping = false;
        return;
      }
      swipeX = e.touches[0].clientX;
      swipeY = e.touches[0].clientY;
      swiping = true;
    },
    { passive: true },
  );
  window.addEventListener(
    "touchmove",
    function (e) {
      if (!swiping || size !== "max") return;
      const dx = e.touches[0].clientX - swipeX;
      const dy = e.touches[0].clientY - swipeY;
      if (dx > 64 && Math.abs(dx) > Math.abs(dy) * 1.6) {
        swiping = false;
        if (port) port.postMessage({ type: "exitMax" });
      }
    },
    { passive: true },
  );
  window.addEventListener(
    "touchend",
    function () {
      swiping = false;
    },
    { passive: true },
  );
})();
