// @aura/chat-core — framework-agnostic chat transport, wire types, and the
// session view-model shared by the web dashboard (web/) and the macOS app
// (app/mac/). `chatWs` is the channel-WS client; `model` is the normalized
// per-session view-model; `transcript` + `frames` build it from inbound
// frames so both frontends stay behaviourally aligned.
export * from './chatWs';
export * from './model';
export * from './transcript';
export * from './frames';
