// Typed wrapper for the direct (non-relay) push-registration command. The chat
// session lifecycle (create / connect / send / history) is transport-agnostic and
// backend-routed, so it lives in the shared `chat_*` commands — this module only
// covers direct-mode push, which has no relay analog on this path.

import { invoke } from "@tauri-apps/api/core";

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
