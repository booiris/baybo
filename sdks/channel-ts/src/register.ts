import { randomUUID } from "node:crypto";
import readline from "node:readline";

import type { PromptKind } from "./generated/PromptKind.js";
import type { RegisterIn } from "./generated/RegisterIn.js";
import type { RegisterOut } from "./generated/RegisterOut.js";

export interface RegistrationContext {
  input(label: string, opts?: { required?: boolean }): Promise<string>;
  password(label: string, opts?: { required?: boolean }): Promise<string>;
}

export interface RegistrationResult {
  botId: string;
  token: string;
}

const MAX_FRAME_BYTES = 64 * 1024;

/**
 * Entry point for a sidecar's registration flow. Invoked by
 * {@link runSidecar} when `AURA_CHANNEL_MODE === "register"`. Drives a
 * line-delimited JSON exchange over stdin/stdout; writes one terminal
 * frame (`result` on success, `error` on throw) before exiting.
 *
 * Human-visible output (QR art, progress) MUST go to `process.stderr`
 * — stdout is protocol-only so the CLI parent can parse it without
 * false positives.
 */
export async function runRegistration(
  fn: (ctx: RegistrationContext) => Promise<RegistrationResult>,
): Promise<never> {
  type Waiter = { resolve: (v: string) => void; reject: (err: Error) => void };
  const pending = new Map<string, Waiter>();
  let stdinClosed = false;

  const failAll = (err: Error): void => {
    for (const w of pending.values()) w.reject(err);
    pending.clear();
  };

  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  rl.on("line", (line) => {
    if (Buffer.byteLength(line, "utf8") > MAX_FRAME_BYTES) {
      failAll(new Error(`host frame exceeds ${MAX_FRAME_BYTES} bytes`));
      return;
    }
    let msg: RegisterIn;
    try {
      msg = JSON.parse(line) as RegisterIn;
    } catch (e) {
      failAll(new Error(`invalid JSON from host: ${(e as Error).message}`));
      return;
    }
    if (msg.type === "prompt_reply") {
      const waiter = pending.get(msg.id);
      if (!waiter) return;
      pending.delete(msg.id);
      waiter.resolve(msg.value);
    } else if (msg.type === "cancel") {
      failAll(new Error("cancelled by host"));
    }
  });
  rl.on("close", () => {
    stdinClosed = true;
    failAll(new Error("host stdin closed"));
  });

  const writeOut = (msg: RegisterOut): Promise<void> =>
    new Promise((resolve, reject) => {
      const line = JSON.stringify(msg) + "\n";
      process.stdout.write(line, (err) => (err ? reject(err) : resolve()));
    });

  const ask =
    (kind: PromptKind) =>
    (label: string, opts?: { required?: boolean }): Promise<string> => {
      if (stdinClosed) return Promise.reject(new Error("host stdin closed"));
      const id = randomUUID();
      return new Promise<string>((resolve, reject) => {
        pending.set(id, { resolve, reject });
        writeOut({
          type: "prompt",
          id,
          label,
          kind,
          required: opts?.required ?? false,
        }).catch((err) => {
          pending.delete(id);
          reject(err);
        });
      });
    };

  const onSignal = (sig: NodeJS.Signals): void => {
    const line =
      JSON.stringify({
        type: "error",
        message: `registration cancelled (${sig})`,
      } satisfies RegisterOut) + "\n";
    process.stdout.write(line, () => process.exit(130));
  };
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);

  const ctx: RegistrationContext = {
    input: ask("input"),
    password: ask("password"),
  };

  try {
    const result = await fn(ctx);
    if (typeof result?.botId !== "string" || typeof result?.token !== "string") {
      throw new Error(
        "register hook must return { botId: string, token: string }",
      );
    }
    await writeOut({
      type: "result",
      bot_id: result.botId,
      token: result.token,
    });
    process.exit(0);
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    try {
      await writeOut({ type: "error", message });
    } catch {
      // swallow — already exiting
    }
    process.exit(1);
  }
}
