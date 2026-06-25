import { useEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
// PairChallenge / PairedSummary are generated from the src-tauri IPC structs by
// ts-rs (cargo test -p baybo-mobile-app --features ts-export); the bindings are
// regenerated + drift-checked by scripts/check-ts-bindings.sh.
import type { PairChallenge } from "./generated/PairChallenge";
import type { PairedSummary } from "./generated/PairedSummary";

/// Parse a `baybo://pair?h=<endpoint>&c=<code>&relay=1` QR payload, or fall back
/// to treating the whole scanned string as the bare code. `relay=1` means join
/// the proxy rendezvous; otherwise dial the gateway directly.
function parseScan(text: string): { endpoint?: string; code: string; relay: boolean } {
  try {
    const url = new URL(text);
    if (url.protocol === "baybo:") {
      const h = url.searchParams.get("h") ?? undefined;
      const c = url.searchParams.get("c");
      const relay = url.searchParams.get("relay") === "1";
      if (c) return { endpoint: h, code: c, relay };
    }
  } catch {
    /* not a URL — treat as a bare code */
  }
  return { code: text.trim(), relay: false };
}

/// A decrypted wire `Frame` as it arrives over the Tauri content channel.
/// MessagePack field names round-trip as snake_case JSON; we only model the few
/// variants the chat view renders and tolerate the rest.
type WireFrame =
  | { kind: "message"; content: string; role?: "user" | "assistant"; platform_msg_id?: string }
  | { kind: "answer_delta"; text: string }
  | { kind: "turn_state"; active: boolean }
  | { kind: "notice"; level: string; text: string }
  | { kind: "reset"; reason: string }
  // Frames we don't render (reasoning, tool progress, ping/pong, …) arrive with
  // other `kind`s and fall through the switch's `default`.
  | { kind: "other" };

type ChatMsg = { id: string; role: "user" | "assistant" | "notice"; content: string };

/// The post-pairing chat: opens a Noise content session for a fresh session id,
/// renders the agent's streamed reply, and sends user messages.
function ChatView({ onClose }: { onClose: () => void }) {
  const [sessionId] = useState(() => crypto.randomUUID());
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  const [input, setInput] = useState("");
  const [status, setStatus] = useState<string | null>("Connecting…");
  // Idempotency keys we've optimistically rendered, so the server's echo of our
  // own message doesn't render it a second time.
  const sentIds = useRef<Set<string>>(new Set());

  useEffect(() => {
    const channel = new Channel<WireFrame>();
    channel.onmessage = (frame) => {
      switch (frame.kind) {
        case "message": {
          const role = frame.role === "user" ? "user" : "assistant";
          if (
            role === "user" &&
            frame.platform_msg_id &&
            sentIds.current.has(frame.platform_msg_id)
          ) {
            return; // our own message, already shown optimistically
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
    invoke("content_connect", { sessionId, onFrame: channel })
      .then(() => setStatus(null))
      .catch((e) => setStatus(`Connect failed: ${e}`));
    return () => {
      invoke("content_disconnect").catch(() => {});
    };
  }, [sessionId]);

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
  const [endpoint, setEndpoint] = useState("wss://proxy.baybo.space");
  const [relay, setRelay] = useState(true);
  const [code, setCode] = useState("");
  const [label, setLabel] = useState("My iPhone");
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [challenge, setChallenge] = useState<PairChallenge | null>(null);
  const [paired, setPaired] = useState<PairedSummary | null>(null);
  // On launch, a persisted pairing means we can skip straight to "connected".
  const [rememberedUser, setRememberedUser] = useState<string | null>(null);
  // Whether the chat view is open (a live content session).
  const [chatting, setChatting] = useState(false);
  // QR scan flow. "scanning" makes the page transparent so the native camera
  // (drawn behind the webview) shows through; "success" plays a brief
  // confirmation beat on the app background, covering the camera teardown so the
  // hand-off to the form doesn't flash. `scanCancelled` suppresses the error
  // toast on a user cancel.
  const [scanPhase, setScanPhase] = useState<"idle" | "scanning" | "success">("idle");
  const scanCancelled = useRef(false);

  useEffect(() => {
    invoke<string | null>("paired_user")
      .then((u) => setRememberedUser(u))
      .catch(() => {});
  }, []);

  useEffect(() => {
    document.documentElement.classList.toggle("scanning", scanPhase === "scanning");
    return () => document.documentElement.classList.remove("scanning");
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
      setCode(parsed.code);
      setRelay(parsed.relay);
      if (parsed.endpoint) setEndpoint(parsed.endpoint);
      // Success: buzz, pop a green dot at the reticle centre, then briefly hold
      // and cross-fade to the form (same background, so no abrupt wipe).
      try {
        const { notificationFeedback } = await import("@tauri-apps/plugin-haptics");
        await notificationFeedback("success");
      } catch {
        // haptics unavailable (e.g. desktop) — non-fatal
      }
      setScanPhase("success");
      await new Promise((resolve) => setTimeout(resolve, 650));
      setStatus("Scanned. Review and pair.");
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
  async function pairBegin() {
    setBusy(true);
    setStatus("Connecting…");
    try {
      const c = await invoke<PairChallenge>("pair_begin", { endpoint, code, label, relay });
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

  if (chatting) {
    return <ChatView onClose={() => setChatting(false)} />;
  }

  if (paired) {
    return (
      <main className="container">
        <h1>Connected</h1>
        <p className="muted">Paired and ready.</p>
        <dl className="kv">
          <dt>User</dt>
          <dd>{paired.userId}</dd>
          <dt>Pairing code</dt>
          <dd>{paired.pairingCode}</dd>
          <dt>Relay node</dt>
          <dd>{paired.relayNodeId || "—"}</dd>
          <dt>Direct candidates</dt>
          <dd>{paired.directCandidates.length ? paired.directCandidates.join(", ") : "—"}</dd>
        </dl>
        <div className="row">
          <button onClick={() => setChatting(true)}>Open chat</button>
          <button onClick={() => setPaired(null)}>Pair another</button>
        </div>
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
      </main>
    );
  }

  if (rememberedUser) {
    return (
      <main className="container">
        <h1>Connected</h1>
        <p className="muted">
          Paired as {rememberedUser} (remembered from a previous session).
        </p>
        <div className="row">
          <button onClick={() => setChatting(true)}>Open chat</button>
          <button onClick={() => setRememberedUser(null)}>Pair another device</button>
        </div>
      </main>
    );
  }

  return (
    <>
      <main className="container">
        <h1>Baybo</h1>
        <p className="muted">Scan the pairing code shown by <code>baybo device pair</code>.</p>

        <label>
          Gateway endpoint
          <input value={endpoint} onChange={(e) => setEndpoint(e.target.value)} placeholder="ws://host:port" />
        </label>
        <label>
          Pairing code
          <input value={code} onChange={(e) => setCode(e.target.value)} placeholder="WORMHOLE-7-…" />
        </label>
        <label>
          Device label
          <input value={label} onChange={(e) => setLabel(e.target.value)} />
        </label>
        <label className="checkbox">
          <input type="checkbox" checked={relay} onChange={(e) => setRelay(e.target.checked)} />
          Connect via relay (proxy)
        </label>

        <div className="row">
          <button onClick={scan} disabled={busy}>Scan QR</button>
          <button onClick={pairBegin} disabled={busy || !code || !endpoint}>Pair</button>
        </div>

        {status && <p className="status">{status}</p>}
      </main>

      {scanPhase === "scanning" && (
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
