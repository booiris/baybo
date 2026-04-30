// Test-only re-export. Bundled by `esbuild.config.mjs` into
// `dist/network_policy_test.mjs` so `node --test` can import the
// functions under test from a self-contained file.
export { expandIpv6, isBlockedIp, resolveAndCheck } from "../src/network_policy.ts";
