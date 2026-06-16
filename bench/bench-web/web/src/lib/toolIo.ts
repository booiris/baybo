import { useSyncExternalStore } from 'react';

/**
 * Global "optimized tool I/O" view preference + the transforms it gates.
 *
 * When on (the default), a tool's params/result render as their *core
 * data*: the JSON envelope (braces/quotes) and the wire `type`
 * discriminant are stripped, a single-field object collapses to just its
 * value, and string values print verbatim — so embedded newlines show as
 * real line breaks instead of escaped `\n`. When off, the raw value is
 * shown as pretty-printed JSON.
 */

const STORAGE_KEY = 'bench-web.tool-io.optimized';

function readInitial(): boolean {
  try {
    // Default on: only an explicit "0" opts out.
    return localStorage.getItem(STORAGE_KEY) !== '0';
  } catch {
    return true;
  }
}

let optimized = readInitial();
const listeners = new Set<() => void>();

export function setOptimizedToolIo(value: boolean): void {
  optimized = value;
  try {
    localStorage.setItem(STORAGE_KEY, value ? '1' : '0');
  } catch {
    // Non-fatal: the preference just won't persist across reloads.
  }
  for (const l of listeners) l();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

export function useOptimizedToolIo(): boolean {
  return useSyncExternalStore(
    subscribe,
    () => optimized,
    () => true,
  );
}

// ── transforms ────────────────────────────────────────────────────────

// Wire-level discriminant keys that carry no information for a reader
// (e.g. the `"type": "text"` tag every tool result object ships).
const NOISE_KEYS = new Set(['type']);

const TOOL_OUTPUT_OPEN = /^<tool_output\b[^>]*>\n?/;
const TOOL_OUTPUT_CLOSE = /\n?<\/tool_output>\s*$/;

/**
 * Render a tool param/result value as compact, human-readable text: drop
 * the JSON envelope and the wire `type` discriminant, omit empty fields,
 * collapse a single remaining field to just its value, and print strings
 * verbatim so real newlines render as line breaks rather than `\n`.
 */
export function optimizeToolValue(value: unknown): string {
  if (value == null) return '';
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  if (Array.isArray(value)) {
    return value
      .map((v) => optimizeToolValue(v))
      .filter((s) => s.length > 0)
      .join('\n');
  }
  if (typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>)
      .filter(([k]) => !NOISE_KEYS.has(k))
      .map(([k, v]) => [k, optimizeToolValue(v)] as const)
      .filter(([, v]) => v.length > 0);
    if (entries.length === 0) return '';
    if (entries.length === 1) return entries[0][1];
    return entries.map(([k, v]) => (v.includes('\n') ? `${k}:\n${v}` : `${k}: ${v}`)).join('\n');
  }
  return String(value);
}

/** Strip aura's `<tool_output name="X">…</tool_output>` wrapper from a tool
 * result string, leaving just the body. No-op when the wrapper is absent. */
export function stripToolOutputWrapper(content: string): string {
  return content.replace(TOOL_OUTPUT_OPEN, '').replace(TOOL_OUTPUT_CLOSE, '');
}
