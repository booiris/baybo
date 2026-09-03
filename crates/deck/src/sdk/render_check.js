// Gate-side render check for the FRONTEND half of a bundle.
//
// The install gate boots `service.js` for real and checks the JSON it returns,
// but `card.html` was never parsed, run, or rendered — so every frontend
// defect reached the user's phone unchecked, and the authoring agent shipped
// code it had no way to observe. This closes that: it runs the card's own
// script against the card's own first snapshot, through the real card SDK,
// and answers one question the shape of the JSON cannot —
//
//     did handing this card its data change anything it displays?
//
// A card that throws, that reads a field its service never emits, or that
// never wires `deck.onData` all fail that question identically, and all three
// look on the phone like a page of placeholder dashes.
//
// This is deliberately NOT a browser: there is no layout, no cascade, no
// paint, so it cannot judge whether a card looks right — only whether it
// responded to its data at all. Layout stays the client's problem.
//
// Usage: bun render_check.js <card.html> <sdkCard.js>   snapshot JSON on stdin.
// Writes one JSON verdict to stdout.

const [cardPath, sdkPath] = process.argv.slice(2);
const cardHtml = await Bun.file(cardPath).text();
const sdkSource = await Bun.file(sdkPath).text();
const snapshot = JSON.parse(await Bun.stdin.text());

// Ids the markup actually declares. `getElementById` returns null for anything
// else — a shim that invents elements would hide exactly the null-dereference
// bugs this exists to catch.
const declaredIds = new Set(
  [...cardHtml.matchAll(/\bid\s*=\s*["']([^"']+)["']/g)].map((m) => m[1]),
);

const errors = [];
const nullLookups = new Set();

// An open MessagePort — and any interval a card sets — keeps bun's loop alive,
// so this process must end itself rather than fall off the end. The gate also
// bounds it from outside; this is the inner guard that turns a wedged card
// into a verdict instead of a timeout.
function verdict(body) {
  console.log(JSON.stringify(body));
  process.exit(0);
}
setTimeout(() => {
  errors.push("render check timed out — the card blocked for 10s");
  verdict({ ok: false, errors, stage: "timeout" });
}, 10_000).unref?.();

class El {
  constructor(tag = "div") {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.attributes = {};
    this.style = {};
    this.dataset = {};
    this.classList = { add() {}, remove() {}, toggle() {}, contains: () => false };
    this.className = "";
    this.id = "";
    this.hidden = false;
    this._text = "";
  }
  get textContent() {
    return this._text;
  }
  set textContent(v) {
    this._text = v == null ? "" : String(v);
    this.children = [];
  }
  get innerHTML() {
    return "";
  }
  set innerHTML(_v) {
    // Cards are told to build DOM with textContent; a card that paints with
    // innerHTML is a security finding, not a render failure, so do not fail
    // the check here — just refuse to model it.
  }
  setAttribute(k, v) {
    this.attributes[k] = String(v);
  }
  getAttribute(k) {
    return k in this.attributes ? this.attributes[k] : null;
  }
  removeAttribute(k) {
    delete this.attributes[k];
  }
  hasAttribute(k) {
    return k in this.attributes;
  }
  appendChild(c) {
    this.children.push(c);
    return c;
  }
  append(...cs) {
    this.children.push(...cs);
  }
  prepend(...cs) {
    this.children.unshift(...cs);
  }
  replaceChildren(...cs) {
    this.children = cs;
  }
  removeChild(c) {
    this.children = this.children.filter((x) => x !== c);
    return c;
  }
  remove() {}
  insertBefore(c) {
    this.children.push(c);
    return c;
  }
  querySelector() {
    return null;
  }
  querySelectorAll() {
    return [];
  }
  addEventListener() {}
  removeEventListener() {}
  getBoundingClientRect() {
    return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
  }
  focus() {}
  blur() {}
  get firstChild() {
    return this.children[0] ?? null;
  }
  get lastChild() {
    return this.children[this.children.length - 1] ?? null;
  }
  get childNodes() {
    return this.children;
  }
  get parentNode() {
    return null;
  }
}

const elements = new Map();
for (const id of declaredIds) {
  const el = new El();
  el.id = id;
  elements.set(id, el);
}

const documentElement = new El("html");
const body = new El("body");
const document = {
  documentElement,
  body,
  head: new El("head"),
  getElementById(id) {
    const found = elements.get(id);
    if (found === undefined) {
      nullLookups.add(id);
      return null;
    }
    return found;
  },
  createElement: (t) => new El(t),
  createElementNS: (_ns, t) => new El(t),
  createTextNode: (t) => {
    const n = new El("#text");
    n.textContent = t;
    return n;
  },
  createDocumentFragment: () => new El("#fragment"),
  querySelector: () => null,
  querySelectorAll: () => [],
  addEventListener() {},
  removeEventListener() {},
};

const messageListeners = [];
const windowStub = {
  document,
  addEventListener(type, fn) {
    if (type === "message") messageListeners.push(fn);
  },
  removeEventListener() {},
  requestAnimationFrame: (fn) => setTimeout(() => fn(0), 0),
  cancelAnimationFrame: (h) => clearTimeout(h),
  setTimeout,
  clearTimeout,
  setInterval,
  clearInterval,
  innerWidth: 390,
  innerHeight: 780,
  devicePixelRatio: 3,
  getComputedStyle: () => ({ getPropertyValue: () => "" }),
  matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
  location: { href: "about:srcdoc" },
  navigator: { userAgent: "baybo-deck-render-check" },
};
windowStub.window = windowStub;
windowStub.parent = windowStub;
windowStub.self = windowStub;

// The card's inline script, exactly as the shell would run it after the SDK.
function scriptOf(html) {
  const out = [];
  for (const m of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/gi)) out.push(m[1]);
  return out.join("\n;\n");
}

// In the real iframe a card reads `deck` as a bare global, because the SDK
// assigned it to `window`. Nothing here makes window properties global, so the
// bindings are passed explicitly — `extras` is how the card script receives
// the `deck` the SDK just published.
const run = (src, label, extras = {}) => {
  const names = [
    "window", "document", "globalThis", "self", "requestAnimationFrame",
    "cancelAnimationFrame", "getComputedStyle", "matchMedia", "navigator", "location",
  ];
  const values = [
    windowStub, document, windowStub, windowStub,
    windowStub.requestAnimationFrame, windowStub.cancelAnimationFrame,
    windowStub.getComputedStyle, windowStub.matchMedia,
    windowStub.navigator, windowStub.location,
  ];
  for (const [name, value] of Object.entries(extras)) {
    names.push(name);
    values.push(value);
  }
  try {
    // eslint-disable-next-line no-new-func
    const fn = new Function(...names, `"use strict";\n${src}`);
    fn(...values);
    return true;
  } catch (e) {
    errors.push(`${label}: ${e && e.message ? e.message : String(e)}`);
    return false;
  }
};

if (!run(sdkSource, "card SDK")) {
  verdict({ ok: false, errors, stage: "sdk" });
}
const deck = windowStub.deck;
if (!deck) {
  verdict({ ok: false, errors: ["card SDK did not expose `deck`"] });
}

if (!run(scriptOf(cardHtml), "card script", { deck })) {
  verdict({ ok: false, errors, stage: "card-script" });
}

// Drive the real handshake the shell performs.
const channel = new MessageChannel();
const fromCard = [];
channel.port1.onmessage = (e) => {
  const m = e.data || {};
  if (m.type === "log" && m.level === "error") errors.push(`card logged: ${m.message}`);
  fromCard.push(m);
};

const snapshotOf = () => {
  const out = {};
  for (const [id, el] of elements) out[id] = el.textContent;
  return out;
};

const initSize = process.env.DECK_RENDER_SIZE || "wide";
for (const fn of messageListeners) {
  try {
    fn({ source: windowStub.parent, data: { type: "deck_init", size: initSize }, ports: [channel.port2] });
  } catch (e) {
    errors.push(`deck_init: ${e && e.message ? e.message : String(e)}`);
  }
}

const settle = () => new Promise((r) => setTimeout(r, 25));
await settle();

const before = snapshotOf();
channel.port1.postMessage({ type: "data", payload: snapshot });
await settle();

// Every other size the card claims to implement: a resize must not throw.
for (const size of (process.env.DECK_RENDER_SIZES || "").split(",").filter(Boolean)) {
  if (size === initSize) continue;
  channel.port1.postMessage({ type: "size", size });
  await settle();
}

const after = snapshotOf();
const changed = Object.keys(after).filter((id) => after[id] !== before[id]);

verdict({
  ok: errors.length === 0 && changed.length > 0,
  errors,
  changed_ids: changed.slice(0, 20),
  changed_count: changed.length,
  declared_ids: elements.size,
  missing_ids: [...nullLookups].slice(0, 20),
});
