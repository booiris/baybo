#!/usr/bin/env python3
"""Generate docs/assets/architecture.svg — regenerate with: python3 docs/assets/gen-architecture-svg.py"""

W, H = 1400, 1452
parts = []
FONT = "ui-sans-serif, system-ui, -apple-system, 'Segoe UI', 'Helvetica Neue', Arial, sans-serif"
MONO = "ui-monospace, 'SF Mono', Menlo, Consolas, monospace"

C = {
    "client":  ("#4f46e5", "#eef2ff"),
    "gateway": ("#0284c7", "#e0f2fe"),
    "runtime": ("#b45309", "#fef3c7"),
    "cap":     ("#047857", "#d1fae5"),
    "obs":     ("#7c3aed", "#ede9fe"),
    "store":   ("#334155", "#e2e8f0"),
    "ext":     ("#be123c", "#fff1f2"),
    "found":   ("#64748b", "#f1f5f9"),
}
INK, SUB = "#0f172a", "#334155"


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def box(x, y, w, h, title, lines, kind, crate=None, dashed=False, tsize=13):
    stroke, fill = C[kind]
    dash = ' stroke-dasharray="5,4"' if dashed else ""
    parts.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="8" fill="{fill}" stroke="{stroke}" stroke-width="1.5"{dash}/>')
    ty = y + 19
    parts.append(f'<text x="{x + w / 2}" y="{ty}" text-anchor="middle" font-family="{FONT}" font-size="{tsize}" font-weight="700" fill="{INK}">{esc(title)}</text>')
    ty += 15
    for ln in lines:
        parts.append(f'<text x="{x + w / 2}" y="{ty}" text-anchor="middle" font-family="{FONT}" font-size="10.5" fill="{SUB}">{esc(ln)}</text>')
        ty += 13
    if crate:
        parts.append(f'<text x="{x + w / 2}" y="{y + h - 7}" text-anchor="middle" font-family="{MONO}" font-size="10" fill="{stroke}">{esc(crate)}</text>')


def band(x, y, w, h, label, crates=None, color="#94a3b8"):
    parts.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="12" fill="none" stroke="{color}" stroke-width="1" stroke-dasharray="2,3"/>')
    parts.append(f'<text x="{x + 12}" y="{y + 18}" font-family="{FONT}" font-size="11.5" font-weight="700" letter-spacing="1.2" fill="{color}">{esc(label.upper())}</text>')
    if crates:
        parts.append(f'<text x="{x + w - 14}" y="{y + 18}" text-anchor="end" font-family="{MONO}" font-size="10.5" fill="{color}">{esc(crates)}</text>')


def arrow(pts, label=None, num=None, color="#475569", width=1.6, dashed=False, lx=None, ly=None):
    d = f"M {pts[0][0]} {pts[0][1]} " + " ".join(f"L {x} {y}" for x, y in pts[1:])
    dash = ' stroke-dasharray="6,4"' if dashed else ""
    parts.append(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="{width}" marker-end="url(#arr)"{dash}/>')
    if label is not None:
        mx = lx if lx is not None else (pts[0][0] + pts[-1][0]) / 2
        my = ly if ly is not None else (pts[0][1] + pts[-1][1]) / 2
        tw = len(label) * 6.4 + (34 if num else 12)
        parts.append(f'<rect x="{mx - tw / 2}" y="{my - 11}" width="{tw}" height="21" rx="10.5" fill="#ffffff" stroke="#cbd5e1" stroke-width="0.8"/>')
        tx = mx + 10 if num else mx
        if num:
            parts.append(f'<circle cx="{mx - tw / 2 + 13}" cy="{my - 0.5}" r="8.5" fill="{INK}"/>')
            parts.append(f'<text x="{mx - tw / 2 + 13}" y="{my + 3}" text-anchor="middle" font-family="{FONT}" font-size="10.5" font-weight="700" fill="#ffffff">{num}</text>')
        parts.append(f'<text x="{tx}" y="{my + 3.5}" text-anchor="middle" font-family="{FONT}" font-size="10.5" font-weight="600" fill="{INK}">{esc(label)}</text>')


# ---------- canvas ----------
parts.append(f'<rect x="0" y="0" width="{W}" height="{H}" fill="#ffffff"/>')
parts.append(f'<text x="40" y="38" font-family="{FONT}" font-size="21" font-weight="800" fill="{INK}">Baybo — module architecture &amp; data flow</text>')
parts.append(f'<text x="40" y="58" font-family="{FONT}" font-size="12" fill="{SUB}">one gateway process, thin clients · numbered arrows 1–7 trace the life of a message · dashed boxes are external processes/services</text>')

# ---------- band A: clients ----------
band(28, 74, 1344, 128, "clients & satellites")
box(48, 102, 196, 72, "Web dashboard", ["embedded React app"], "client", "app/web")
box(264, 102, 178, 72, "Terminal UI", ["baybo tui, over WS"], "client", "crates/tui")
box(462, 102, 160, 72, "One-shot CLI", ["baybo prompt"], "client", "crates/cli")
box(642, 102, 190, 72, "Telegram / WeChat", ["bots via bun sidecars"], "client", "sidecars/channel/*")
box(852, 102, 236, 72, "remote-host relay", ["blind relay + push · separate deploy"], "ext", "remote-host/", dashed=True)
box(1108, 102, 244, 72, "iOS companion", ["SwiftUI app · E2E encrypted"], "client", "app/ios")

# ---------- band B: gateway ----------
band(28, 224, 1344, 136, "gateway — one axum process", "crates/gateway · crates/channels · crates/wire")
box(48, 266, 226, 72, "Admin API + Web UI", ["/v1/* REST + SSE (bearer auth)"], "gateway")
box(294, 266, 210, 72, "Channel WebSocket", ["/v1/channel-ws · all chat surfaces"], "gateway")
box(524, 266, 178, 72, "Blob side-channel", ["attachments by reference"], "gateway")
box(722, 266, 188, 72, "Pairing gate", ["unknown senders need approval"], "gateway", "crates/pairing")
box(930, 266, 202, 72, "Sidecar supervisor", ["runs the bun channel sidecars"], "gateway")
box(1152, 266, 200, 72, "ChannelRegistry", ["dispatches events to every surface"], "gateway", "crates/channels")

# client → gateway arrows
arrow([(146, 174), (146, 266)])
arrow([(353, 174), (353, 266)], "inbound message", num="1", ly=206, lx=430)
arrow([(542, 174), (542, 218), (480, 218), (480, 266)])
arrow([(737, 174), (737, 266)])
arrow([(1108, 138), (1088, 138)], color="#94a3b8", dashed=True)   # iOS → relay
arrow([(913, 174), (913, 266)], color="#94a3b8", dashed=True)     # relay → gateway

# ---------- band C: runtime + observability rail ----------
band(28, 382, 1032, 404, "agent runtime", "crates/agent (assembly)")
box(48, 426, 288, 64, "Router", ["gates each message,", "resolves the session"], "runtime")
box(366, 426, 306, 64, "AgentActor — one per session", ["priority mailbox · serializes", "a session's work"], "runtime")
box(702, 426, 336, 64, "SecurityGateway", ["sanitizes all LLM + tool I/O"], "runtime")
box(48, 522, 288, 64, "AgentSupervisor", ["actor registry + idle reaper", "(actors only, never session rows)"], "runtime")
box(366, 522, 306, 64, "AgentLoop", ["context → LLM → tools,", "one Turn per unit of work"], "runtime")
box(702, 522, 336, 64, "ToolExecutor", ["approval gate · OS sandbox for shell"], "runtime", "crates/sandbox · crates/process")
box(48, 618, 288, 64, "ContextManager", ["transcript window · budget · compaction"], "runtime", "crates/context")
box(366, 618, 306, 64, "LLM client pool", ["19 providers · retries · cost guard"], "runtime", "crates/llm")
box(702, 618, 336, 56, "Session state", ["SessionManager: get-or-create, folders"], "runtime", "crates/session")
box(702, 702, 336, 48, "LLM provider APIs", ["Anthropic · OpenAI · Gemini · DeepSeek …"], "ext", dashed=True)

band(1080, 382, 292, 404, "observability")
box(1098, 426, 256, 64, "Turn", ["turn state machine + event bus"], "obs", "crates/turn")
box(1098, 518, 256, 64, "Trace", ["Session > Turn > Step > Span tree"], "obs", "crates/trace")
box(1098, 610, 256, 64, "Cost", ["per-call spend, integer micro-USD"], "obs", "crates/cost")
box(1098, 702, 256, 64, "Query", ["read-only analytics"], "obs", "crates/query")

# gateway ↔ runtime arrows
arrow([(300, 360), (300, 426)], "pairing + gates", num="2", ly=371, lx=330)
arrow([(96, 426), (96, 360)], "stream deltas out", num="6", ly=371, lx=110)
arrow([(336, 458), (366, 458)], "enqueue", num="3", lx=351, ly=458)
arrow([(192, 490), (192, 522)])                                   # Router → Supervisor
arrow([(336, 545), (351, 545), (351, 474), (366, 474)])           # Supervisor spawn → Actor
arrow([(519, 490), (519, 522)])                                   # Actor → Loop
arrow([(519, 586), (519, 618)], "context + LLM", num="4", ly=602, lx=575)
arrow([(336, 645), (366, 645)])                                   # ContextManager ↔ pool
arrow([(672, 554), (702, 554)], "tools", num="5", lx=687, ly=554)
arrow([(870, 490), (870, 522)])                                   # SecurityGateway ↔ executor
arrow([(870, 586), (870, 618)], dashed=True, color="#94a3b8")     # executor ↔ session state
arrow([(672, 674), (687, 674), (687, 726), (702, 726)], dashed=True, color="#94a3b8")  # pool → provider APIs
arrow([(1038, 554), (1098, 554)], "events", lx=1067, ly=535)

# ---------- band D: capabilities ----------
band(28, 806, 1344, 216, "capability crates — each owns its Tool impls; registries are the seams")
r1y, r2y = 850, 938
bw = 208
xs = [48, 270, 492, 714, 936, 1158]
box(xs[0], r1y, bw, 64, "tools", ["built-in tool set + MCP client"], "cap", "crates/tools")
box(xs[1], r1y, bw, 64, "skills", ["SKILL.md packages, risk-assessed"], "cap", "crates/skills")
box(xs[2], r1y, bw, 64, "subagent", ["spawn_subagent → child sessions"], "cap", "crates/subagent")
box(xs[3], r1y, bw, 64, "cron", ["schedules → new sessions"], "cap", "crates/cron")
box(xs[4], r1y, bw, 64, "memory", ["built-in + pluggable memory"], "cap", "crates/memory")
box(xs[5], r1y, bw, 64, "project", ["kanban boards · git worktrees"], "cap", "crates/project")
box(xs[0], r2y, bw, 64, "task", ["planning checklist"], "cap", "crates/task")
box(xs[1], r2y, bw, 64, "search", ["web search providers"], "cap", "crates/search")
box(xs[2], r2y, bw, 64, "deck", ["live cards for iOS"], "cap", "crates/deck")
box(xs[3], r2y, bw, 64, "External MCP servers", ["tools appear as <server>/<tool>"], "ext", dashed=True)
box(xs[4], r2y, bw, 64, "claude / codex CLIs", ["external subagent backends"], "ext", dashed=True)
box(xs[5], r2y, bw, 64, "browser sidecar", ["opt-in browser tools"], "ext", dashed=True)

# capability plumbing
arrow([(152, 850), (152, 796), (1050, 796), (1050, 570), (1038, 570)], "tool registry", lx=600, ly=796)
arrow([(818, 850), (818, 782), (40, 782), (40, 458), (48, 458)], "cron fires re-enter the Router", color="#94a3b8", dashed=True, lx=430, ly=782)
arrow([(200, 914), (200, 926), (810, 926), (810, 938)], dashed=True, color="#94a3b8")   # tools → MCP servers
arrow([(600, 914), (600, 931), (1030, 931), (1030, 938)], dashed=True, color="#94a3b8") # subagent → claude/codex

# ---------- band E: persistence ----------
band(28, 1042, 1344, 184, "persistence & workspace")
box(48, 1082, 280, 56, "store — the ports", ["every *Store trait contract"], "store", "crates/store")
box(48, 1158, 280, 56, "storage — sqlite adapter", ["one sqlite file behind all stores"], "store", "crates/storage")
box(358, 1082, 300, 56, "Secret vault", ["encrypted keys, tokens, creds"], "store", "crates/security")
box(358, 1158, 300, 56, "janitor", ["TTL maintenance sweeps"], "store", "crates/janitor")
box(688, 1082, 380, 132, "Workspace filesystem  ~/.baybo/", ["personas/ (identity, skills, memory)", "config/ · agents/ · deck/ · work/ · logs/"], "store", "crates/workspace")
box(1098, 1082, 256, 132, "Session data is core data", ["sessions & transcripts are never", "deleted — append-only, compaction", "supersedes rows"], "store")

arrow([(188, 1138), (188, 1158)])
arrow([(220, 1022), (220, 1082)], "persist: transcript · turns · traces · cost", num="7", ly=1052, lx=430)

# ---------- band F: foundation ----------
band(28, 1246, 1344, 100, "shared foundation — types only, no business logic")
box(48, 1274, 310, 58, "model", ["shared domain types"], "found", "crates/model")
box(379, 1274, 310, 58, "config", ["config schema + validation"], "found", "crates/config")
box(710, 1274, 310, 58, "wire", ["channel frame protocol"], "found", "crates/wire")
box(1041, 1274, 310, 58, "device-proto", ["device pairing crypto"], "found", "crates/device-proto")

# legend
ly0 = 1386
parts.append(f'<text x="40" y="{ly0}" font-family="{FONT}" font-size="11" font-weight="700" fill="{SUB}">Legend:</text>')
lx0 = 100
for kind, label in [("client", "clients"), ("gateway", "gateway"), ("runtime", "agent runtime"), ("obs", "observability"), ("cap", "capabilities"), ("store", "persistence"), ("found", "shared foundation"), ("ext", "external (dashed)")]:
    stroke, fill = C[kind]
    parts.append(f'<rect x="{lx0}" y="{ly0 - 12}" width="26" height="15" rx="4" fill="{fill}" stroke="{stroke}" stroke-width="1.4"/>')
    parts.append(f'<text x="{lx0 + 32}" y="{ly0}" font-family="{FONT}" font-size="11" fill="{SUB}">{esc(label)}</text>')
    lx0 += 40 + len(label) * 6.6 + 24

# flow legend — what the numbered badges mean
fy = 1422
parts.append(f'<text x="40" y="{fy}" font-family="{FONT}" font-size="11" font-weight="700" fill="{SUB}">The life of a message:</text>')
fx = 178
for n, lbl in [("1", "inbound"), ("2", "pairing + gates"), ("3", "enqueue (turn opens)"),
               ("4", "context + LLM call"), ("5", "tool execution"), ("6", "stream reply out"),
               ("7", "persist: transcript / turn / trace / cost")]:
    parts.append(f'<circle cx="{fx}" cy="{fy - 4}" r="8.5" fill="{INK}"/>')
    parts.append(f'<text x="{fx}" y="{fy - 0.5}" text-anchor="middle" font-family="{FONT}" font-size="10.5" font-weight="700" fill="#ffffff">{n}</text>')
    parts.append(f'<text x="{fx + 13}" y="{fy}" font-family="{FONT}" font-size="11" fill="{SUB}">{esc(lbl)}</text>')
    fx += 13 + len(lbl) * 6.1 + 14
    if n != "7":
        parts.append(f'<text x="{fx}" y="{fy}" font-family="{FONT}" font-size="11" fill="#94a3b8">&#8594;</text>')
        fx += 24

svg = (
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}">'
    '<defs><marker id="arr" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
    '<path d="M 0 0 L 10 5 L 0 10 z" fill="#475569"/></marker></defs>'
    + "".join(parts)
    + "</svg>"
)
import pathlib
pathlib.Path("docs/assets").mkdir(exist_ok=True)
pathlib.Path("docs/assets/architecture.svg").write_text(svg)
print("written", len(svg), "bytes")
