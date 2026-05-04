import test from "node:test";
import assert from "node:assert/strict";
import { Readable } from "node:stream";

import { BlobPairingRequiredError } from "@aura/channel-sdk";
import { downloadResourceAsAttachment } from "../dist/media/inbound.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

// Cap defined in src/media/inbound.ts. Tests assert against this
// value via the public behaviour (under-cap accepted, over-cap dropped).
const MAX_RESOURCE_BYTES = 50 * 1024 * 1024;

const sampleResource = (overrides = {}) => ({
  type: "image",
  fileKey: "img_abc",
  ...overrides,
});

// Build a fake LarkChannel whose rawClient.im.v1.{image,file}.get returns
// the configured payload as a Readable stream. `bytes` may be a Buffer
// (delivered as one chunk), an array of Buffers (multi-chunk), or an
// Error (the get() call rejects with it).
function stubChannel({ bytes, headers, throwsOnGet, chunks }) {
  const get = async () => {
    if (throwsOnGet) throw throwsOnGet;
    const stream = chunks
      ? Readable.from(chunks)
      : Readable.from(bytes ?? Buffer.alloc(0));
    return {
      headers: headers ?? {},
      getReadableStream: () => stream,
    };
  };
  return {
    rawClient: {
      im: {
        v1: {
          image: { get },
          file: { get },
        },
      },
    },
  };
}

// `uploadBlob` and `checkPairing` read AURA_CHANNEL_URL/AURA_CHANNEL_TOKEN
// and hit different paths on the gateway HTTP surface. Intercept with
// a single fetch override that dispatches by URL path so each test only
// has to supply the blob handler — the pairing preflight defaults to
// `{ paired: true }` (the steady-state for an already-paired peer).
// Tests that exercise the unpaired branch override `pairing` explicitly.
function withStubGateway(handler, fn, opts = {}) {
  const prevUrl = process.env["AURA_CHANNEL_URL"];
  const prevToken = process.env["AURA_CHANNEL_TOKEN"];
  const prevFetch = globalThis.fetch;
  process.env["AURA_CHANNEL_URL"] = "ws://127.0.0.1:1/v1/channel-ws";
  process.env["AURA_CHANNEL_TOKEN"] = "test-token";
  const pairingHandler =
    opts.pairing
    ?? (async () =>
      new Response(JSON.stringify({ paired: true }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }));
  globalThis.fetch = async (url, init) => {
    const u = typeof url === "string" ? url : url.toString();
    if (u.includes("/v1/pairing/check")) {
      return pairingHandler(url, init);
    }
    return handler(url, init);
  };
  return Promise.resolve(fn()).finally(() => {
    if (prevUrl === undefined) delete process.env["AURA_CHANNEL_URL"];
    else process.env["AURA_CHANNEL_URL"] = prevUrl;
    if (prevToken === undefined) delete process.env["AURA_CHANNEL_TOKEN"];
    else process.env["AURA_CHANNEL_TOKEN"] = prevToken;
    globalThis.fetch = prevFetch;
  });
}

const okBlobResponse = () =>
  new Response(JSON.stringify({ blob_id: "blob-1" }), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

test("downloadResourceAsAttachment: under-cap stream uploads and returns attachment", async () => {
  await withStubGateway(async () => okBlobResponse(), async () => {
    const channel = stubChannel({
      chunks: [Buffer.from("png-"), Buffer.from("bytes")],
    });
    const out = await downloadResourceAsAttachment({
      channel,
      resource: sampleResource(),
      botId: "cli_a1",
      userId: "lark_cli_a1_chatId=oc_x_ou_alice",
      logger: noopLogger,
    });
    assert.deepEqual(out, {
      kind: "image",
      blob_id: "blob-1",
      mime_type: "image/jpeg",
      size: 9,
    });
  });
});

test("downloadResourceAsAttachment: pre-flight content-length over cap drops without reading", async () => {
  let uploadAttempted = false;
  await withStubGateway(
    async () => {
      uploadAttempted = true;
      return okBlobResponse();
    },
    async () => {
      let streamCreated = false;
      const channel = {
        rawClient: {
          im: {
            v1: {
              image: {
                async get() {
                  return {
                    headers: { "content-length": String(MAX_RESOURCE_BYTES + 1) },
                    getReadableStream: () => {
                      streamCreated = true;
                      return Readable.from(Buffer.alloc(0));
                    },
                  };
                },
              },
              file: { async get() { throw new Error("unreachable"); } },
            },
          },
        },
      };
      const out = await downloadResourceAsAttachment({
        channel,
        resource: sampleResource(),
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.equal(out, null);
      assert.equal(streamCreated, false, "stream was opened despite oversize header");
      assert.equal(uploadAttempted, false);
    },
  );
});

test("downloadResourceAsAttachment: mid-stream cap exceeded aborts and returns null", async () => {
  let uploadAttempted = false;
  await withStubGateway(
    async () => {
      uploadAttempted = true;
      return okBlobResponse();
    },
    async () => {
      // Two chunks where the SECOND pushes past the cap. Use a small
      // synthetic cap by overriding the resource size: the helper's
      // cap is 50 MiB, so create chunks just over that boundary.
      const chunkSize = 25 * 1024 * 1024 + 1;
      const channel = stubChannel({
        chunks: [
          Buffer.alloc(chunkSize, 0xab),
          Buffer.alloc(chunkSize, 0xcd),
        ],
      });
      const out = await downloadResourceAsAttachment({
        channel,
        resource: sampleResource({ type: "file", fileKey: "file_xyz" }),
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.equal(out, null);
      assert.equal(uploadAttempted, false);
    },
  );
});

test("downloadResourceAsAttachment: rawClient get() rejection returns null (no upload)", async () => {
  let uploadAttempted = false;
  await withStubGateway(
    async () => {
      uploadAttempted = true;
      return okBlobResponse();
    },
    async () => {
      const channel = stubChannel({ throwsOnGet: new Error("network blip") });
      const out = await downloadResourceAsAttachment({
        channel,
        resource: sampleResource(),
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.equal(out, null);
      assert.equal(uploadAttempted, false);
    },
  );
});

test("downloadResourceAsAttachment: pairing_required rethrows BlobPairingRequiredError", async () => {
  await withStubGateway(
    async () =>
      new Response(
        JSON.stringify({ error: "pairing_required", code: "AAAA-1234" }),
        { status: 403, headers: { "content-type": "application/json" } },
      ),
    async () => {
      await assert.rejects(
        () =>
          downloadResourceAsAttachment({
            channel: stubChannel({ bytes: Buffer.from("png-bytes") }),
            resource: sampleResource(),
            botId: "cli_a1",
            userId: "lark_cli_a1_chatId=oc_x_ou_alice",
            logger: noopLogger,
          }),
        (err) => {
          assert.ok(err instanceof BlobPairingRequiredError);
          assert.equal(err.code, "AAAA-1234");
          return true;
        },
      );
    },
  );
});

test("downloadResourceAsAttachment: unpaired triple skips the Lark CDN download entirely", async () => {
  // Codex review #2: an unpaired `(channel_type, bot_id, user_id)`
  // must not be able to force a 50 MiB download from Lark before
  // `uploadBlob`'s pairing gate rejects. The preflight against
  // `/v1/pairing/check` short-circuits before `rawClient.im.*.get`
  // ever runs.
  let getCalled = false;
  let uploadAttempted = false;
  const channel = {
    rawClient: {
      im: {
        v1: {
          image: {
            async get() {
              getCalled = true;
              throw new Error("download path must not be hit when unpaired");
            },
          },
          file: { async get() { throw new Error("unreachable"); } },
        },
      },
    },
  };
  await withStubGateway(
    async () => {
      uploadAttempted = true;
      return okBlobResponse();
    },
    async () => {
      const out = await downloadResourceAsAttachment({
        channel,
        resource: sampleResource(),
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_unpaired",
        logger: noopLogger,
      });
      assert.equal(out, null, "unpaired returns null without an attachment");
      assert.equal(getCalled, false, "rawClient.image.get must not run on unpaired triple");
      assert.equal(uploadAttempted, false, "uploadBlob must not run on unpaired triple");
    },
    {
      pairing: async () =>
        new Response(JSON.stringify({ paired: false }), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
    },
  );
});

test("downloadResourceAsAttachment: pairing preflight failure falls through to download", async () => {
  // Network flake on the preflight must not drop legitimate media —
  // the `uploadBlob` path still gates the durable write, so falling
  // through is safe and avoids a paired user losing their attachment
  // because of a transient gateway hiccup.
  let getCalled = false;
  let uploadAttempted = false;
  const channel = stubChannel({ chunks: [Buffer.from("png-bytes")] });
  // Wrap the channel's get so we can observe whether it ran.
  const wrappedGet = channel.rawClient.im.v1.image.get;
  channel.rawClient.im.v1.image.get = async (...args) => {
    getCalled = true;
    return wrappedGet(...args);
  };
  await withStubGateway(
    async () => {
      uploadAttempted = true;
      return okBlobResponse();
    },
    async () => {
      const out = await downloadResourceAsAttachment({
        channel,
        resource: sampleResource(),
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.ok(out, "fall-through path still returns an attachment");
      assert.equal(getCalled, true);
      assert.equal(uploadAttempted, true);
    },
    {
      pairing: async () =>
        new Response("internal", {
          status: 500,
          headers: { "content-type": "text/plain" },
        }),
    },
  );
});

test("downloadResourceAsAttachment: non-pairing upload error returns null (logged, not thrown)", async () => {
  await withStubGateway(
    async () =>
      new Response("nope", {
        status: 500,
        headers: { "content-type": "text/plain" },
      }),
    async () => {
      const out = await downloadResourceAsAttachment({
        channel: stubChannel({ bytes: Buffer.from("png-bytes") }),
        resource: sampleResource(),
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.equal(out, null);
    },
  );
});

test("downloadResourceAsAttachment: file resource type uses rawClient.im.v1.file.get", async () => {
  let imagePathHit = false;
  let filePathHit = false;
  const channel = {
    rawClient: {
      im: {
        v1: {
          image: {
            async get() {
              imagePathHit = true;
              return { headers: {}, getReadableStream: () => Readable.from(Buffer.alloc(0)) };
            },
          },
          file: {
            async get() {
              filePathHit = true;
              return {
                headers: {},
                getReadableStream: () =>
                  Readable.from([Buffer.from("audio-bytes")]),
              };
            },
          },
        },
      },
    },
  };
  await withStubGateway(async () => okBlobResponse(), async () => {
    const out = await downloadResourceAsAttachment({
      channel,
      resource: sampleResource({ type: "audio", fileKey: "audio_xyz" }),
      botId: "cli_a1",
      userId: "lark_cli_a1_chatId=oc_x_ou_alice",
      logger: noopLogger,
    });
    assert.ok(out, "audio resource should produce an attachment");
    assert.equal(out.kind, "audio");
    assert.equal(filePathHit, true);
    assert.equal(imagePathHit, false);
  });
});
