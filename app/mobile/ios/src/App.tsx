import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/// Mirror of `PairedSummary` returned by the `pair` command (src-tauri).
type PairedSummary = {
  userId: string;
  relayNodeId: string;
  directCandidates: string[];
  pairingCode: string;
  pendingApproval: boolean;
};

/// Parse an `baybo://pair?h=<endpoint>&c=<code>` QR payload, or fall back to
/// treating the whole scanned string as the bare code.
function parseScan(text: string): { endpoint?: string; code: string } {
  try {
    const url = new URL(text);
    if (url.protocol === "baybo:") {
      const h = url.searchParams.get("h") ?? undefined;
      const c = url.searchParams.get("c");
      if (c) return { endpoint: h, code: c };
    }
  } catch {
    /* not a URL — treat as a bare code */
  }
  return { code: text.trim() };
}

export default function App() {
  const [endpoint, setEndpoint] = useState("ws://127.0.0.1:8889");
  const [code, setCode] = useState("");
  const [label, setLabel] = useState("My iPhone");
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [paired, setPaired] = useState<PairedSummary | null>(null);

  async function scan() {
    setStatus("Opening scanner…");
    try {
      const { scan, Format } = await import("@tauri-apps/plugin-barcode-scanner");
      const res = await scan({ windowed: true, formats: [Format.QRCode] });
      const parsed = parseScan(res.content);
      setCode(parsed.code);
      if (parsed.endpoint) setEndpoint(parsed.endpoint);
      setStatus("Scanned. Review and pair.");
    } catch (e) {
      setStatus(`Scan failed: ${e}`);
    }
  }

  async function pair() {
    setBusy(true);
    setStatus("Pairing…");
    try {
      const summary = await invoke<PairedSummary>("pair", { endpoint, code, label });
      setPaired(summary);
      setStatus(null);
    } catch (e) {
      setStatus(`Pairing failed: ${e}`);
    } finally {
      setBusy(false);
    }
  }

  if (paired) {
    return (
      <main className="container">
        <h1>Connected</h1>
        <p className="muted">
          {paired.pendingApproval
            ? "Waiting for the operator to approve this device. Your token is inert until then."
            : "Approved and ready."}
        </p>
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
        <button onClick={() => setPaired(null)}>Pair another</button>
      </main>
    );
  }

  return (
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

      <div className="row">
        <button onClick={scan} disabled={busy}>Scan QR</button>
        <button onClick={pair} disabled={busy || !code || !endpoint}>Pair</button>
      </div>

      {status && <p className="status">{status}</p>}
    </main>
  );
}
