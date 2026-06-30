// Typed wrappers for the direct (non-relay) chat transport's Tauri commands.
// The connect/send/disconnect calls share the transport-agnostic `chat_*`
// commands (routed by a `leg` arg, dispatched inline in ChatView); this module
// covers the direct-only REST calls — minting a gateway chat session and
// refetching a transcript slice.
//
// The gateway REST shapes come from the SAME OpenAPI spec `app/web` consumes
// (`docs/openapi.json` → `src/generated/schema.d.ts`, via `pnpm gen:api`), so
// these types track the gateway source of truth instead of being hand-mirrored.
// The Rust direct leg speaks the same endpoints (`POST /v1/chat/sessions`,
// `GET /v1/chat/sessions/{id}`); it forwards the history JSON verbatim, so the
// type lands here on the webview side.

import { invoke } from "@tauri-apps/api/core";

import type { components } from "./generated/schema";

/** `POST /v1/chat/sessions` (+ `.../{id}/token`) response — the session id and
 * its capability token. Generated from the gateway's OpenAPI spec. */
export type ChatSessionCredential = components["schemas"]["ChatSessionCredential"];
/** `GET /v1/chat/sessions/{id}` response — a transcript slice the webview rebuilds
 * the thread from after a `Frame::Reset`. Generated from the gateway's OpenAPI spec. */
export type ChatSessionDetail = components["schemas"]["ChatSessionDetail"];

/**
 * Mint a fresh direct chat session over REST (admin Bearer) and return its
 * gateway-assigned session id. The channel token is stashed Rust-side for the WS
 * + blob legs (the webview never sees it), so the IPC return is just the id.
 * Unlike the relay path (which uses a client-minted UUID), the direct session id
 * is owned by the gateway, so it must come from here.
 */
export async function directSessionCreate(): Promise<string> {
  const { session_id } = await invoke<Pick<ChatSessionCredential, "session_id">>(
    "direct_session_create",
  );
  return session_id;
}

/**
 * Refetch a transcript slice (`GET /v1/chat/sessions/{id}`, admin Bearer) — the
 * direct-path recovery for a WS `Frame::Reset`, where the gateway tells the client
 * to rebuild the thread from REST. Pages older with `beforeOrdinal`; `limit` caps
 * the slice. The Rust leg forwards the gateway JSON verbatim, typed here as the
 * generated {@link ChatSessionDetail}.
 */
export async function directHistory(
  sessionId: string,
  beforeOrdinal?: number,
  limit?: number,
): Promise<ChatSessionDetail> {
  return invoke<ChatSessionDetail>("direct_history", { sessionId, beforeOrdinal, limit });
}

/**
 * Best-effort: provision (or refresh) this app's direct-mode push binding with
 * the connected gateway, so a backgrounded direct chat can still buzz. The Rust
 * side no-ops when iOS hasn't issued an APNs token yet, or the gateway has no
 * `[push]` remote host configured — so this is safe to call repeatedly (on
 * connect and on every foreground). Never throws: push is non-essential, and
 * blocking the UI on it would be wrong.
 */
export async function directPushRegister(): Promise<void> {
  try {
    await invoke("direct_push_register");
  } catch {
    /* push is best-effort — swallow */
  }
}
