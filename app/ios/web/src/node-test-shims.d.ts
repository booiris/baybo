// This bundle targets WKWebView, so `tsconfig.json` deliberately ships no
// node lib and no `@types/node` — making `process` / `Buffer` ambient in a
// browser bundle is the mistake that omission prevents. A vitest suite still
// runs in node, and one of them reads a stylesheet off disk (vitest stubs
// `?raw` css imports to ""), so declare exactly that one import and nothing
// else. Grow this only for another test-only node API, never for src.
declare module "node:fs" {
  export function readFileSync(path: string, encoding: "utf8"): string;
}
