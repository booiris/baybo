/**
 * AES-128-ECB primitives for Weixin CDN media. iLink ships every
 * media payload encrypted with a 16-byte key under ECB + PKCS7 — both
 * upload and download share the same crypto, so the key/IV-less
 * helpers here are deliberately tiny and pure-function. We never use
 * AES-ECB for anything but iLink CDN; the lack of an IV is forced by
 * the protocol, not a security choice we get to make.
 */
import { createCipheriv, createDecipheriv } from "node:crypto";

/** Encrypt buffer with AES-128-ECB. PKCS7 padding is the OpenSSL default. */
export function encryptAesEcb(plaintext: Buffer, key: Buffer): Buffer {
  const cipher = createCipheriv("aes-128-ecb", key, null);
  return Buffer.concat([cipher.update(plaintext), cipher.final()]);
}

/** Decrypt buffer with AES-128-ECB. PKCS7 padding is auto-stripped. */
export function decryptAesEcb(ciphertext: Buffer, key: Buffer): Buffer {
  const decipher = createDecipheriv("aes-128-ecb", key, null);
  return Buffer.concat([decipher.update(ciphertext), decipher.final()]);
}

/**
 * AES-128-ECB ciphertext size for a given plaintext size. PKCS7 always
 * pads up to the next 16-byte boundary — even when the plaintext is
 * already a multiple of 16, an extra block of pad bytes is appended.
 * The iLink server uses this in the upload preflight.
 */
export function aesEcbPaddedSize(plaintextSize: number): number {
  return Math.ceil((plaintextSize + 1) / 16) * 16;
}
