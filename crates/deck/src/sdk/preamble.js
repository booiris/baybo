// Deck service preamble — trusted runtime, versioned with the gateway
// (see SDK_VERSION in baybo-deck). Owns the stdio NDJSON protocol so the
// agent-written service module is pure logic:
//
//   export const ops = { quota: async (params, ctx) => ({...}) }
//   export function start(ctx) { /* own timer, ctx.emit(...) */ }
//
// ctx surface (universal, undeclared — see docs/modules/deck.md):
//   ctx.fetch(url, {method, headers, body, bodyBlob}) -> {status, headers, body, json()}
//     host-mediated: SSRF floor + [{REDACTED_SECRET_…}] reveal happen in
//     the Rust parent. The child itself is trusted host code and can perform
//     direct I/O, but secrets remain parent-only. `bodyBlob` streams a stored
//     blob as the request body (the bytes never enter the child).
//   ctx.exec(cmd) -> {code, stdout, stderr}
//     host /bin/sh -c, inherited environment, 10s wall clock + output caps
//   ctx.emit(payload)                          policed + seq'd by the parent
//   ctx.log(msg)
//   --- blobs (docs/modules/deck.md §Blobs), ref-first so bytes stay out of bun:
//   ctx.fetchBlob(url, {method, headers, body}) -> {blobId, contentType, size}
//     host fetches the URL and streams the response straight into the blob
//     store (2xx only; bounded redirects); the card gets only the ref.
//   ctx.blobPut(base64, contentType) -> {blobId, contentType, size}
//     store inline card-produced bytes (small — capped for the stdio line).
//   ctx.blobPutFile(path, contentType) -> {blobId, contentType, size}
//     store an exec-produced file (relative to the exec scratch cwd, streamed).
//   ctx.blobGet(blobId) -> {base64, contentType, size}   read back (capped).
//
// Run as: bun preamble.js /abs/path/to/service.js

let nextHostId = 1;
const pendingHost = new Map();

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

// Console must not corrupt the protocol stream.
for (const level of ["log", "info", "warn", "error", "debug"]) {
  console[level] = (...args) =>
    send({ type: "log", level, msg: args.map((a) => String(a)).join(" ") });
}

function hostRequest(msg) {
  return new Promise((resolve, reject) => {
    const id = nextHostId++;
    pendingHost.set(id, { resolve, reject });
    send({ ...msg, id });
  });
}

async function hostFetch(url, opts = {}) {
  const res = await hostRequest({
    type: "fetch",
    url: String(url),
    method: opts.method || "GET",
    headers: opts.headers || {},
    body: opts.body == null ? null : String(opts.body),
    body_blob: opts.bodyBlob == null ? null : String(opts.bodyBlob),
  });
  return {
    status: res.status,
    headers: res.headers,
    body: res.body,
    json() {
      return JSON.parse(res.body);
    },
  };
}

// The Rust host serializes blob refs snake_case; present the card a camelCase
// ref so the SDK boundary owns the wire shape (it can evolve behind this).
function toBlobRef(v) {
  return { blobId: v.blob_id, contentType: v.content_type, size: v.size };
}

async function hostFetchBlob(url, opts = {}) {
  return toBlobRef(
    await hostRequest({
      type: "fetchBlob",
      url: String(url),
      method: opts.method || "GET",
      headers: opts.headers || {},
      body: opts.body == null ? null : String(opts.body),
    }),
  );
}

const ctx = {
  fetch: hostFetch,
  exec: (cmd) => hostRequest({ type: "exec", cmd: String(cmd) }),
  emit: (payload) => send({ type: "emit", payload }),
  log: (msg) => send({ type: "log", level: "info", msg: String(msg) }),
  fetchBlob: hostFetchBlob,
  blobPut: async (base64, contentType) =>
    toBlobRef(
      await hostRequest({
        type: "blobPut",
        base64: String(base64),
        content_type: contentType == null ? null : String(contentType),
      }),
    ),
  blobPutFile: async (path, contentType) =>
    toBlobRef(
      await hostRequest({
        type: "blobPutFile",
        path: String(path),
        content_type: contentType == null ? null : String(contentType),
      }),
    ),
  blobGet: (blobId) => hostRequest({ type: "blobGet", blob_id: String(blobId) }),
};

let mod = null;

async function handle(msg) {
  if (msg.type === "host_result") {
    const p = pendingHost.get(msg.id);
    if (!p) return;
    pendingHost.delete(msg.id);
    if (msg.ok) p.resolve(msg.value);
    else p.reject(new Error(msg.error || "host error"));
    return;
  }
  if (msg.type === "init") {
    try {
      mod = await import(process.argv[2]);
      if (!mod || typeof mod.ops !== "object" || mod.ops === null) {
        send({ type: "fatal", error: "service.js must `export const ops = {...}`" });
        process.exit(64);
      }
      send({ type: "ready" });
      if (typeof mod.start === "function") {
        Promise.resolve(mod.start(ctx)).catch((e) =>
          send({
            type: "log",
            level: "error",
            msg: "start() failed: " + ((e && e.stack) || e),
          }),
        );
      }
    } catch (e) {
      send({ type: "fatal", error: String((e && e.stack) || e) });
      process.exit(64);
    }
    return;
  }
  if (msg.type === "call") {
    const { id, op, params } = msg;
    try {
      const fn = mod && mod.ops ? mod.ops[op] : undefined;
      if (typeof fn !== "function") throw new Error("op not implemented: " + op);
      const value = await fn(params ?? {}, ctx);
      send({ type: "result", id, ok: true, value: value === undefined ? null : value });
    } catch (e) {
      send({ type: "result", id, ok: false, error: String((e && e.stack) || e) });
    }
  }
}

let buf = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buf += chunk;
  let idx;
  while ((idx = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, idx);
    buf = buf.slice(idx + 1);
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    handle(msg);
  }
});
process.stdin.on("end", () => process.exit(0));
