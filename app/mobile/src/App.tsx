import { useCallback, useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
// PairChallenge / PairedSummary are generated from the src-tauri IPC structs by
// ts-rs (cargo test -p baybo-mobile-app --features ts-export); the bindings are
// regenerated + drift-checked by scripts/check-ts-bindings.sh.
import type { PairAborted } from "./generated/PairAborted";
import type { PairChallenge } from "./generated/PairChallenge";
import type { PairedSummary } from "./generated/PairedSummary";

/// Foreground signals (page visibility, window focus, the native `app-resumed`
/// event) can fire 2–3 times on a single iOS resume; coalesce them into one
/// reconnect so we don't open several content-join legs. The Rust shell also
/// guards against a concurrent dial, but debouncing here avoids queueing the work
/// at all.
const FOREGROUND_RECONNECT_DEBOUNCE_MS = 400;

/// Parse a `baybo://pair?h=<relay>&r=<rendezvous-id>&s=<secret>&k=<remote-api-key>`
/// QR payload. Both `r` (public rendezvous id) and `s` (the 256-bit secret, the
/// Noise PSK) are required — there is no typeable fallback, because a short
/// secret would be offline-crackable by a hostile relay. Pairing is relay-only:
/// the app joins the proxy rendezvous (`/pair/join/<rendezvous-id>`) presenting
/// `k` as the relay admission key.
function parseScan(text: string): {
  endpoint?: string;
  rendezvousId: string;
  secret: string;
  remoteApiKey?: string;
} | null {
  try {
    const url = new URL(text);
    if (url.protocol === "baybo:") {
      const h = url.searchParams.get("h") ?? undefined;
      const r = url.searchParams.get("r");
      const s = url.searchParams.get("s");
      const k = url.searchParams.get("k") ?? undefined;
      if (r && s) return { endpoint: h, rendezvousId: r, secret: s, remoteApiKey: k };
    }
  } catch {
    /* not a pairing URL */
  }
  // No bare-code fallback: a pairing QR must carry both the rendezvous id and
  // the high-entropy secret.
  return null;
}

/// A decrypted wire `Frame` as it arrives over the Tauri content channel.
/// MessagePack field names round-trip as snake_case JSON; we only model the few
/// variants the chat view renders and tolerate the rest.
type WireFrame =
  | {
      kind: "message";
      content: string;
      role?: "user" | "assistant";
      platform_msg_id?: string;
      // The reply's persisted row ordinal — the catch-up cursor on reconnect.
      // Present on durable rows (live final messages + replayed history).
      ordinal?: number;
    }
  | { kind: "answer_delta"; text: string }
  | { kind: "turn_state"; active: boolean }
  | { kind: "notice"; level: string; text: string }
  | { kind: "reset"; reason: string }
  // Frames we don't render (reasoning, tool progress, ping/pong, …) arrive with
  // other `kind`s and fall through the switch's `default`.
  | { kind: "other" };

type ChatMsg = { id: string; role: "user" | "assistant" | "notice"; content: string };

// Pairing defaults now that the manual form is gone: the QR carries the relay
// endpoint (`h=`), so we only fall back to the public proxy when a bare-code QR
// omits it.
const DEFAULT_ENDPOINT = "wss://proxy.baybo.space";

// The windowed scanner draws the camera behind the webview and exposes no
// "camera ready" signal, so going transparent up front would flash a black frame
// while the feed warms up. Instead we hold an opaque cover over the page for one
// AVCaptureSession startup, then reveal the camera + reticle together. This is a
// timed approximation — a true ready gate would need a native event from the
// plugin (it has none).
const CAMERA_WARMUP_MS = 300;

// True under `tauri ios dev` (Vite dev server) and `tauri ios build --debug`,
// false in release. Gates the on-screen scan readout below — never shown in a
// shipped build.
const DEBUG = import.meta.env.DEV || import.meta.env.TAURI_ENV_DEBUG === "true";

// Reveal only a short prefix + length of a secret, so the debug readout proves
// what was scanned (and lets you cross-check the prefix against the terminal)
// without printing the full pairing code on screen.
function maskSecret(s: string): string {
  if (s.length <= 4) return s.length ? "•".repeat(s.length) : "—";
  return `${s.slice(0, 3)}…(${s.length})`;
}

// Chat persistence. iOS reclaims a backgrounded WKWebView's content process (and
// can relaunch the app), which reloads the page and wipes React state. So the
// active chat is mirrored to localStorage (disk-backed — survives a reload and a
// cold start); on remount we restore it instantly, then reconnect and replay only
// the gap above `lastOrdinal`, so a background round-trip lands back in the live
// chat instead of the landing screen.
const CHAT_ACTIVE_KEY = "baybo.chat.active";
const CHAT_SESSION_KEY = "baybo.chat.session";
const CHAT_STATE_KEY = "baybo.chat.state";

type ChatState = { sessionId: string; messages: ChatMsg[]; lastOrdinal: number };

function loadChatState(sessionId: string): ChatState | null {
  try {
    const raw = localStorage.getItem(CHAT_STATE_KEY);
    if (!raw) return null;
    const s = JSON.parse(raw) as ChatState;
    // Ignore a thread persisted under a different session id (stale).
    if (s.sessionId !== sessionId || !Array.isArray(s.messages)) return null;
    return s;
  } catch {
    return null;
  }
}

function saveChatState(s: ChatState) {
  try {
    localStorage.setItem(CHAT_STATE_KEY, JSON.stringify(s));
  } catch {
    /* quota exceeded / storage disabled — persistence is best-effort */
  }
}

function clearChatState() {
  try {
    localStorage.removeItem(CHAT_ACTIVE_KEY);
    localStorage.removeItem(CHAT_SESSION_KEY);
    localStorage.removeItem(CHAT_STATE_KEY);
  } catch {
    /* ignore */
  }
}

/// The post-pairing chat: opens a Noise content session for `sessionId`, renders
/// the agent's streamed reply, and sends user messages. Survives a background
/// round-trip: the thread is persisted, and the session reconnects + replays the
/// gap whenever the app returns to the foreground.
function ChatView({ sessionId, onClose }: { sessionId: string; onClose: () => void }) {
  // Parsed once on mount (lazy initializer), then reused for the initial state.
  const [restored] = useState(() => loadChatState(sessionId));
  const [messages, setMessages] = useState<ChatMsg[]>(restored?.messages ?? []);
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  const [input, setInput] = useState("");
  const [status, setStatus] = useState<string | null>("Connecting…");
  // platform_msg_ids already rendered (our optimistic sends + anything restored),
  // so the server's echo or a catch-up replay doesn't render them twice.
  const sentIds = useRef<Set<string>>(
    new Set((restored?.messages ?? []).filter((m) => m.role === "user").map((m) => m.id)),
  );
  // Highest durable ordinal rendered — the cursor a reconnect catches up from.
  const lastOrdinal = useRef<number>(restored?.lastOrdinal ?? 0);

  // Mirror the thread to disk on every change so a reload/relaunch restores it.
  useEffect(() => {
    saveChatState({ sessionId, messages, lastOrdinal: lastOrdinal.current });
  }, [sessionId, messages]);

  // (Re)open the content session, replaying only the gap above what we've already
  // rendered. Re-run on every foreground: iOS suspends the WS (and may reclaim the
  // webview) in the background, so a resume needs a fresh connection to go live
  // again. Catch-up keys on the ordinal, so reconnecting when nothing changed
  // appends nothing — a cheap no-op.
  const connect = useCallback(() => {
    const channel = new Channel<WireFrame>();
    channel.onmessage = (frame) => {
      switch (frame.kind) {
        case "message": {
          // Advance the cursor first — even for our own echo we dedup below — so a
          // later reconnect doesn't re-replay this row.
          if (typeof frame.ordinal === "number" && frame.ordinal > lastOrdinal.current) {
            lastOrdinal.current = frame.ordinal;
          }
          const role = frame.role === "user" ? "user" : "assistant";
          if (
            role === "user" &&
            frame.platform_msg_id &&
            sentIds.current.has(frame.platform_msg_id)
          ) {
            return; // our own message / already rendered
          }
          if (role === "user" && frame.platform_msg_id) {
            sentIds.current.add(frame.platform_msg_id);
          }
          setStreaming("");
          setMessages((m) => [
            ...m,
            { id: frame.platform_msg_id || crypto.randomUUID(), role, content: frame.content },
          ]);
          break;
        }
        case "answer_delta":
          setStreaming((s) => s + frame.text);
          break;
        case "turn_state":
          setTurnActive(frame.active);
          break;
        case "notice":
          setMessages((m) => [
            ...m,
            { id: crypto.randomUUID(), role: "notice", content: frame.text },
          ]);
          break;
        case "reset":
          setStatus(`Stream reset: ${frame.reason}`);
          break;
        default:
          break; // reasoning / tool progress / etc. not surfaced in phase 1
      }
    };
    setStatus("Connecting…");
    invoke("content_connect", {
      sessionId,
      sinceOrdinal: lastOrdinal.current > 0 ? lastOrdinal.current : null,
      onFrame: channel,
    })
      .then(() => setStatus(null))
      .catch((e) => setStatus(`Connect failed: ${e}`));
  }, [sessionId]);

  useEffect(() => {
    connect(); // first connect is immediate — no foreground burst to coalesce yet

    // iOS suspends the app without ever marking the WKWebView page hidden, so the
    // page's own `visibilitychange` never fires on resume — the chat would keep
    // using a relay leg the OS froze. Reconnect on every foreground signal we can
    // get: page visibility (covers a reloaded webview and dev/browser), window
    // `focus`, and — the reliable one on iOS — the native `app-resumed` event the
    // Rust shell emits from `RunEvent::Resumed`. A single resume fires several of
    // these, so debounce them into one reconnect.
    let timer: ReturnType<typeof setTimeout> | undefined;
    const scheduleConnect = () => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = undefined;
        connect();
      }, FOREGROUND_RECONNECT_DEBOUNCE_MS);
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") scheduleConnect();
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", scheduleConnect);
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen("app-resumed", () => scheduleConnect())
      .then((un) => {
        // The effect may have already torn down before listen() resolved.
        if (cancelled) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", scheduleConnect);
      unlisten?.();
      invoke("content_disconnect").catch(() => {});
    };
  }, [connect]);

  async function send() {
    const text = input.trim();
    if (!text) return;
    const msgId = crypto.randomUUID();
    sentIds.current.add(msgId);
    setMessages((m) => [...m, { id: msgId, role: "user", content: text }]);
    setInput("");
    try {
      await invoke("content_send", { text, msgId });
    } catch (e) {
      setStatus(`Send failed: ${e}`);
    }
  }

  return (
    <main className="container chat">
      <div className="chat-header">
        <button onClick={onClose}>← Back</button>
        <h1>Chat</h1>
      </div>
      <div className="chat-log">
        {messages.map((m) => (
          <div key={m.id} className={`bubble ${m.role}`}>
            {m.content}
          </div>
        ))}
        {streaming && <div className="bubble assistant streaming">{streaming}</div>}
        {turnActive && !streaming && <div className="bubble assistant muted">…</div>}
      </div>
      {status && <p className="status">{status}</p>}
      <div className="row composer">
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") send();
          }}
          placeholder="Message…"
        />
        <button onClick={send} disabled={!input.trim()}>
          Send
        </button>
      </div>
    </main>
  );
}

export default function App() {
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [challenge, setChallenge] = useState<PairChallenge | null>(null);
  const [paired, setPaired] = useState<PairedSummary | null>(null);
  // On launch, a persisted pairing means we can skip straight to "connected".
  const [rememberedDevice, setRememberedDevice] = useState<string | null>(null);
  // Whether the chat view is open, and the active session id. Both are restored
  // from localStorage so a backgrounded webview reload / app relaunch drops the
  // user straight back into the live chat rather than the landing screen.
  const [chatting, setChatting] = useState(() => localStorage.getItem(CHAT_ACTIVE_KEY) === "1");
  const [sessionId, setSessionId] = useState(() => localStorage.getItem(CHAT_SESSION_KEY) ?? "");
  // QR scan flow. "scanning" makes the page transparent so the native camera
  // (drawn behind the webview) shows through; "success" plays a brief
  // confirmation beat on the app background, covering the camera teardown so the
  // jump straight into the handshake doesn't flash. `scanCancelled` suppresses
  // the error toast on a user cancel.
  const [scanPhase, setScanPhase] = useState<"idle" | "scanning" | "success">("idle");
  // Flips true once the camera has had a beat to warm up: drops the warm-up cover
  // and mounts the reticle, revealing the already-transparent live feed.
  const [cameraUp, setCameraUp] = useState(false);
  // Debug-only readout of the last scanned QR (sensitive code masked).
  const [scanInfo, setScanInfo] = useState<string | null>(null);
  const scanCancelled = useRef(false);
  // One app binds one gateway, so re-pairing or unpairing is an explicit,
  // confirmed action. `null` = showing the normal connected actions; otherwise
  // the in-progress confirm for replacing or forgetting the current gateway.
  const [pendingAction, setPendingAction] = useState<null | "replace" | "forget">(null);

  useEffect(() => {
    invoke<string | null>("paired_device")
      .then((d) => setRememberedDevice(d))
      .catch(() => {});
  }, []);

  // Stay transparent for the whole scan so the camera (drawn behind the webview)
  // can show through. The warm-up cover below masks the not-yet-ready feed, so
  // this never reveals a black frame — and because the reveal is just dropping
  // that cover (a React element, in sync with paint) rather than toggling this
  // class in an effect (which paints one frame late), there's no white flash.
  useEffect(() => {
    document.documentElement.classList.toggle("scanning", scanPhase === "scanning");
    return () => document.documentElement.classList.remove("scanning");
  }, [scanPhase]);

  // Drop the cover (and mount the reticle) one warm-up beat after scanning
  // starts; the cleanup clears the pending timer and re-arms the cover the moment
  // scanning ends — on a scanned code, a cancel, or unmount.
  useEffect(() => {
    if (scanPhase !== "scanning") {
      setCameraUp(false);
      return;
    }
    const t = setTimeout(() => setCameraUp(true), CAMERA_WARMUP_MS);
    return () => clearTimeout(t);
  }, [scanPhase]);

  async function scan() {
    scanCancelled.current = false;
    try {
      const bs = await import("@tauri-apps/plugin-barcode-scanner");
      let perm: string = await bs.checkPermissions();
      if (perm === "prompt" || perm === "prompt-with-rationale") {
        perm = await bs.requestPermissions();
      }
      if (perm !== "granted") {
        setStatus("Camera access is off — enable it for Baybo in Settings, then try again.");
        await bs.openAppSettings().catch(() => {});
        return;
      }
      // Go transparent only once permission is granted, right before the camera
      // opens behind the webview.
      setStatus(null);
      setScanPhase("scanning");
      const res = await bs.scan({ windowed: true, formats: [bs.Format.QRCode] });
      const parsed = parseScan(res.content);
      if (!parsed) {
        setStatus("That QR isn't a Baybo pairing code. Scan the one shown by `baybo device pair`.");
        return;
      }
      if (DEBUG) {
        setScanInfo(
          `QR · host=${parsed.endpoint ?? "(default)"} · secret=${maskSecret(parsed.secret)}`,
        );
      }
      // Success: buzz, pop a green dot at the reticle centre, then briefly hold
      // and cross-fade out (same background, so no abrupt wipe).
      try {
        const { notificationFeedback } = await import("@tauri-apps/plugin-haptics");
        await notificationFeedback("success");
      } catch {
        // haptics unavailable (e.g. desktop) — non-fatal
      }
      setScanPhase("success");
      await new Promise((resolve) => setTimeout(resolve, 650));
      // Everything the handshake needs rides in on the QR — go straight into it
      // instead of dropping the user back on a form to review and confirm.
      await pairBegin({
        endpoint: parsed.endpoint ?? DEFAULT_ENDPOINT,
        rendezvousId: parsed.rendezvousId,
        secret: parsed.secret,
        remoteApiKey: parsed.remoteApiKey,
      });
    } catch (e) {
      setStatus(scanCancelled.current ? null : `Scan failed: ${e}`);
    } finally {
      setScanPhase("idle");
    }
  }

  async function cancelScan() {
    scanCancelled.current = true;
    try {
      const { cancel } = await import("@tauri-apps/plugin-barcode-scanner");
      await cancel();
    } catch {
      // already stopped
    }
    setScanPhase("idle");
  }

  // Phase 1: connect + SPAKE2 → get the confirmation code to show the user.
  // Called straight off a successful scan with the QR's endpoint/code.
  async function pairBegin(opts: {
    endpoint: string;
    rendezvousId: string;
    secret: string;
    remoteApiKey?: string;
  }) {
    setBusy(true);
    setStatus("Connecting…");
    // The gateway can cancel pairing (the operator declined, or the link dropped)
    // while we sit on the confirm screen. Listen for that so the screen dismisses
    // itself instead of hanging until the user taps.
    const onAbort = new Channel<PairAborted>();
    onAbort.onmessage = (ev) => {
      setChallenge(null);
      setStatus(`Pairing cancelled: ${ev.reason}`);
    };
    try {
      const c = await invoke<PairChallenge>("pair_begin", {
        endpoint: opts.endpoint,
        rendezvousId: opts.rendezvousId,
        secret: opts.secret,
        remoteApiKey: opts.remoteApiKey,
        onAbort,
      });
      setChallenge(c);
      setStatus(null);
    } catch (e) {
      setStatus(`Pairing failed: ${e}`);
    } finally {
      setBusy(false);
    }
  }

  // Phase 2: send the user's decision; on accept the gateway finalizes once the
  // operator confirms too.
  async function confirmPair(accepted: boolean) {
    if (!challenge) return;
    setBusy(true);
    setStatus(accepted ? "Confirming…" : "Cancelling…");
    try {
      if (accepted) {
        const summary = await invoke<PairedSummary>("pair_confirm", {
          deviceId: challenge.deviceId,
          accepted: true,
        });
        setPaired(summary);
        setChallenge(null);
        setStatus(null);
      } else {
        // The decline path intentionally returns an error on the Rust side.
        await invoke("pair_confirm", { deviceId: challenge.deviceId, accepted: false }).catch(
          () => {},
        );
        setChallenge(null);
        setStatus("Pairing cancelled.");
      }
    } catch (e) {
      setStatus(`Pairing failed: ${e}`);
      setChallenge(null);
    } finally {
      setBusy(false);
    }
  }

  // Unpair: clear the keychain record + push key, then drop back to the scan
  // screen fully unpaired.
  async function forgetPairing() {
    setBusy(true);
    try {
      await invoke("forget_pairing");
      setStatus(null);
    } catch (e) {
      setStatus(`Couldn't forget the pairing: ${e}`);
    } finally {
      setBusy(false);
      setPendingAction(null);
      setPaired(null);
      setRememberedDevice(null);
      // No gateway → no chat: drop any persisted session so it can't resurrect.
      clearChatState();
      setSessionId("");
      setChatting(false);
    }
  }

  // Open a fresh chat: a new session id, persisted with the active flag (and the
  // previous thread dropped), so a mid-chat reload restores *this* conversation.
  function openChat() {
    const id = crypto.randomUUID();
    clearChatState();
    localStorage.setItem(CHAT_SESSION_KEY, id);
    localStorage.setItem(CHAT_ACTIVE_KEY, "1");
    setSessionId(id);
    setChatting(true);
  }

  // Leave the chat (explicit Back): forget the persisted session so the next open
  // starts clean and a later reload won't resurrect a chat the user closed.
  function closeChat() {
    clearChatState();
    setSessionId("");
    setChatting(false);
  }

  // Re-pair: go to the scan screen to bind a different gateway. The current
  // pairing stays in the keychain until the new one finishes (replace-on-
  // success), so backing out of the scan leaves the existing binding intact.
  function startReplace() {
    setPendingAction(null);
    setPaired(null);
    setRememberedDevice(null);
  }

  // The actions on a "connected" screen (shared by the just-paired and the
  // remembered-on-launch views). One app binds one gateway, so the only ways
  // forward are: open the chat, replace this gateway, or forget it.
  function connectedActions() {
    if (pendingAction === "forget") {
      return (
        <div className="confirm-inline">
          <p className="muted">
            Forget this gateway? Notifications and chat stop until you pair again.
          </p>
          <div className="row">
            <button onClick={() => setPendingAction(null)} disabled={busy}>
              Cancel
            </button>
            <button className="danger" onClick={forgetPairing} disabled={busy}>
              Forget
            </button>
          </div>
        </div>
      );
    }
    if (pendingAction === "replace") {
      return (
        <div className="confirm-inline">
          <p className="muted">
            Replace this pairing? You'll scan a new gateway; the current one stays
            until the new pairing finishes.
          </p>
          <div className="row">
            <button onClick={() => setPendingAction(null)} disabled={busy}>
              Cancel
            </button>
            <button onClick={startReplace} disabled={busy}>
              Replace
            </button>
          </div>
        </div>
      );
    }
    return (
      <div className="row">
        <button onClick={openChat}>Open chat</button>
        <button onClick={() => setPendingAction("replace")}>Replace pairing</button>
        <button className="danger" onClick={() => setPendingAction("forget")}>
          Forget
        </button>
      </div>
    );
  }

  if (chatting && sessionId) {
    return <ChatView sessionId={sessionId} onClose={closeChat} />;
  }

  if (paired) {
    return (
      <main className="container">
        <h1>Connected</h1>
        <p className="muted">Paired and ready.</p>
        <dl className="kv">
          <dt>Rendezvous</dt>
          <dd>{paired.rendezvousId}</dd>
          <dt>Relay node</dt>
          <dd>{paired.relayNodeId || "—"}</dd>
        </dl>
        {connectedActions()}
        {status && <p className="status">{status}</p>}
      </main>
    );
  }

  if (challenge) {
    return (
      <main className="container">
        <h1>Confirm pairing</h1>
        <p className="muted">
          Check this code matches the one shown on the computer running{" "}
          <code>baybo device pair</code>, then pair on both.
        </p>
        <div className="confirm-code">{challenge.confirmCode}</div>
        <div className="row">
          <button onClick={() => confirmPair(false)} disabled={busy}>
            Cancel
          </button>
          <button onClick={() => confirmPair(true)} disabled={busy}>
            Pair
          </button>
        </div>
        {status && <p className="status">{status}</p>}
        {DEBUG && scanInfo && <p className="scan-debug">{scanInfo}</p>}
      </main>
    );
  }

  if (rememberedDevice) {
    return (
      <main className="container">
        <h1>Connected</h1>
        <p className="muted">
          Paired (remembered from a previous session).
        </p>
        {connectedActions()}
        {status && <p className="status">{status}</p>}
      </main>
    );
  }

  return (
    <>
      <main className="container">
        <h1>Baybo</h1>
        <p className="muted">Scan the pairing code shown by <code>baybo device pair</code>.</p>

        <div className="row">
          <button onClick={scan} disabled={busy}>Scan QR</button>
        </div>

        {status && <p className="status">{status}</p>}
        {DEBUG && scanInfo && <p className="scan-debug">{scanInfo}</p>}
      </main>

      {scanPhase === "scanning" && !cameraUp && <div className="scan-warming" />}

      {scanPhase === "scanning" && cameraUp && (
        <div className="scan-overlay">
          <svg
            className="scan-reticle"
            viewBox="0 0 100 100"
            fill="none"
            stroke="#fff"
            strokeWidth="8"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M8 24 L8 22 Q8 8 22 8 L24 8" />
            <path d="M92 24 L92 22 Q92 8 78 8 L76 8" />
            <path d="M8 76 L8 78 Q8 92 22 92 L24 92" />
            <path d="M92 76 L92 78 Q92 92 78 92 L76 92" />
          </svg>
          <button className="scan-cancel" onClick={cancelScan}>
            Cancel
          </button>
        </div>
      )}

      {scanPhase === "success" && (
        <div className="scan-panel">
          <span className="scan-dot" />
        </div>
      )}
    </>
  );
}
