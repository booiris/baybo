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
  const queuedCalls = [];
  const pending = new Map();
  let nextId = 1;

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
      size = msg.size;
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
