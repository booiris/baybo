/**
 * Decode WeChat SILK voice bytes to a self-contained WAV (PCM s16le)
 * payload. Most LLM/audio pipelines don't speak SILK directly; WAV is
 * the lowest-common-denominator container that downstream audio tools
 * universally accept.
 *
 * WeChat voice messages are encoded at 24 kHz mono SILK. silk-wasm
 * needs that rate hint to decode; we keep it as a constant rather than
 * threading per-message metadata through, since iLink doesn't surface
 * a sample-rate field on the voice payload anyway.
 */
import { decode as silkDecode, isSilk } from "silk-wasm";

import type { Logger } from "@aura/channel-sdk";

const WECHAT_SILK_SAMPLE_RATE = 24_000;
const WAV_HEADER_BYTES = 44;
const PCM_FORMAT_CODE = 1;
const PCM_BITS_PER_SAMPLE = 16;
const MONO_CHANNELS = 1;

/**
 * Returns a self-contained WAV buffer (PCM s16le, mono, 24 kHz) when
 * `bytes` is decodable SILK; `null` when the bytes don't look like
 * SILK or silk-wasm rejected them. Callers fall back to the raw
 * `audio/silk` path on `null`.
 */
export async function silkToWav(
  bytes: Buffer,
  logger: Logger,
): Promise<Buffer | null> {
  if (!isSilk(bytes)) {
    logger.warn("inbound voice: payload is not SILK; skipping transcode");
    return null;
  }
  try {
    const { data } = await silkDecode(bytes, WECHAT_SILK_SAMPLE_RATE);
    return wrapPcm16leAsWav(Buffer.from(data), WECHAT_SILK_SAMPLE_RATE);
  } catch (err) {
    logger.warn(`inbound voice: silk decode failed (${String(err)}); falling back to raw silk`);
    return null;
  }
}

/**
 * Build a canonical 44-byte RIFF/WAVE header in front of `pcm` and
 * return the concatenated buffer. Mono, 16-bit. Audio pipelines that
 * sniff the WAV magic at offset 0 see a complete file.
 */
function wrapPcm16leAsWav(pcm: Buffer, sampleRate: number): Buffer {
  const byteRate =
    sampleRate * MONO_CHANNELS * (PCM_BITS_PER_SAMPLE / 8);
  const blockAlign = MONO_CHANNELS * (PCM_BITS_PER_SAMPLE / 8);
  const dataLength = pcm.byteLength;

  const header = Buffer.alloc(WAV_HEADER_BYTES);
  // RIFF chunk descriptor
  header.write("RIFF", 0, "ascii");
  header.writeUInt32LE(36 + dataLength, 4);
  header.write("WAVE", 8, "ascii");
  // fmt sub-chunk
  header.write("fmt ", 12, "ascii");
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(PCM_FORMAT_CODE, 20);
  header.writeUInt16LE(MONO_CHANNELS, 22);
  header.writeUInt32LE(sampleRate, 24);
  header.writeUInt32LE(byteRate, 28);
  header.writeUInt16LE(blockAlign, 32);
  header.writeUInt16LE(PCM_BITS_PER_SAMPLE, 34);
  // data sub-chunk
  header.write("data", 36, "ascii");
  header.writeUInt32LE(dataLength, 40);

  return Buffer.concat([header, pcm]);
}
