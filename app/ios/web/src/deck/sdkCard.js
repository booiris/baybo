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
})();
