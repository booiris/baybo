import test from "node:test";
import assert from "node:assert/strict";

import { BlobPairingRequiredError } from "@aura/channel-sdk";
import { downloadResourceAsAttachment } from "../dist/media/inbound.js";

const noopLogger = {
  debug() {},
  info() {},
  warn() {},
  error() {},
};

const sampleResource = {
  type: "image",
  fileKey: "img_abc",
};

// Wrap a download that succeeds; let the test override the upload
// behaviour by stubbing the blobs side-channel via env injection.
function stubChannel(downloadResult) {
  return {
    async downloadResource() {
      if (downloadResult instanceof Error) throw downloadResult;
      return downloadResult;
    },
  };
}

// `uploadBlob` reads AURA_CHANNEL_URL/AURA_CHANNEL_TOKEN; intercept the
// gateway with a one-shot fetch override. Resetting at the end keeps
// the env hygienic for sibling tests.
function withStubGateway(handler, fn) {
  const prevUrl = process.env["AURA_CHANNEL_URL"];
  const prevToken = process.env["AURA_CHANNEL_TOKEN"];
  const prevFetch = globalThis.fetch;
  process.env["AURA_CHANNEL_URL"] = "ws://127.0.0.1:1/v1/channel-ws";
  process.env["AURA_CHANNEL_TOKEN"] = "test-token";
  globalThis.fetch = handler;
  return Promise.resolve(fn()).finally(() => {
    if (prevUrl === undefined) delete process.env["AURA_CHANNEL_URL"];
    else process.env["AURA_CHANNEL_URL"] = prevUrl;
    if (prevToken === undefined) delete process.env["AURA_CHANNEL_TOKEN"];
    else process.env["AURA_CHANNEL_TOKEN"] = prevToken;
    globalThis.fetch = prevFetch;
  });
}

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
            channel: stubChannel(Buffer.from("png-bytes")),
            resource: sampleResource,
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

test("downloadResourceAsAttachment: non-pairing upload error returns null (logged, not thrown)", async () => {
  await withStubGateway(
    async () =>
      new Response("nope", {
        status: 500,
        headers: { "content-type": "text/plain" },
      }),
    async () => {
      const out = await downloadResourceAsAttachment({
        channel: stubChannel(Buffer.from("png-bytes")),
        resource: sampleResource,
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.equal(out, null);
    },
  );
});

test("downloadResourceAsAttachment: download failure returns null (no upload attempt)", async () => {
  let uploadAttempted = false;
  await withStubGateway(
    async () => {
      uploadAttempted = true;
      return new Response(JSON.stringify({ blob_id: "blob-1" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    },
    async () => {
      const out = await downloadResourceAsAttachment({
        channel: stubChannel(new Error("network blip")),
        resource: sampleResource,
        botId: "cli_a1",
        userId: "lark_cli_a1_chatId=oc_x_ou_alice",
        logger: noopLogger,
      });
      assert.equal(out, null);
      assert.equal(uploadAttempted, false);
    },
  );
});
