// Card-side SDK — inlined by the shell into every card iframe's srcdoc
// BEFORE the agent-written fragment, so `deck` exists synchronously for
// card inline scripts. The card runs in an opaque origin with a CSP
// that blocks all network; its only I/O is the MessagePort the shell
// transfers at init. Card code never touches the handshake.
(function () {
  "use strict";
  let port = null;
  let size = "wide";
  let latest;
  let hasData = false;
  const dataCbs = [];
  const sizeCbs = [];
  const queuedCalls = [];
  const pending = new Map();
  let nextId = 1;

  function notifySize() {
    for (const cb of sizeCbs) {
      try {
        cb(size);
      } catch (err) {
        if (port)
          port.postMessage({ type: "log", level: "error", message: String(err) });
      }
    }
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
    } else if (msg.type === "call_result") {
      const p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.value);
      else p.reject(new Error(msg.error || "call failed"));
    } else if (msg.type === "size") {
      if (msg.size && msg.size !== size) {
        size = msg.size;
        notifySize();
      }
    }
  }

  window.addEventListener("message", function (e) {
    if (e.data && e.data.type === "deck_init" && e.ports && e.ports[0]) {
      port = e.ports[0];
      if (e.data.size) size = e.data.size;
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
    get size() {
      return size;
    },
  };

  // Swipe-right-to-exit while maximized. The card content scrolls only
  // vertically, so a clearly-horizontal rightward drag is an unambiguous
  // "collapse" intent — reported to the shell, which runs the restore
  // animation. Touches inside this iframe never reach the shell document, so
  // the gesture must be detected here in the injected SDK, not the shell.
  let swipeX = 0;
  let swipeY = 0;
  let swiping = false;
  window.addEventListener(
    "touchstart",
    function (e) {
      if (size !== "max" || e.touches.length !== 1) {
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
