import { invoke } from '@tauri-apps/api/core';

/** Loopback connection to the in-process gateway, handed over from the Rust
 *  side via the `get_connection` command (docs/mac-app.md §2). */
export interface Connection {
  baseUrl: string;
  adminToken: string;
}

/** Poll `get_connection` until the embedded runtime has booted — it returns
 *  the `not_ready` error until the gateway is bound (docs/mac-app.md §6). */
export async function getConnection(signal?: AbortSignal): Promise<Connection> {
  for (;;) {
    if (signal?.aborted) throw new Error('aborted');
    try {
      return await invoke<Connection>('get_connection');
    } catch (e) {
      if (!String(e).includes('not_ready')) throw e;
      await new Promise((r) => setTimeout(r, 250));
    }
  }
}
