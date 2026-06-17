import { invoke } from '@tauri-apps/api/core';

// Typed wrappers over the bespoke setup commands (src-tauri/src/setup.rs).
// docs/mac-app.md §5.

export interface SetupStatus {
  configured: boolean;
  workspacePath: string;
  gitAvailable: boolean;
}

export interface ProviderInfo {
  id: string;
  defaultBaseUrl: string | null;
  defaultApiKeyEnv: string | null;
}

export interface ModelInfo {
  id: string;
  displayName: string | null;
  contextWindow: number | null;
}

export const setupStatus = () => invoke<SetupStatus>('setup_status');

export const listProviders = () => invoke<ProviderInfo[]>('list_providers');

export const submitApiKey = (provider: string, apiKey: string, baseUrl?: string) =>
  invoke<{ entryName: string }>('submit_api_key', {
    req: { provider, apiKey, baseUrl: baseUrl ?? null },
  });

export const listModels = (provider: string) =>
  invoke<ModelInfo[]>('list_models', { req: { provider } });

export const finishSetup = (args: {
  provider: string;
  entryName: string;
  model: string;
  baseUrl?: string;
  reasoningEffort?: string;
}) =>
  invoke<{ ok: boolean }>('finish_setup', {
    req: {
      provider: args.provider,
      entryName: args.entryName,
      model: args.model,
      baseUrl: args.baseUrl ?? null,
      reasoningEffort: args.reasoningEffort ?? null,
    },
  });

export const startOauth = () =>
  invoke<{ entryName: string; email: string | null; plan: string | null }>('start_oauth');

export const startRuntime = () => invoke<void>('start_runtime');

/** Providers that authenticate via OAuth rather than an API key. */
export const OAUTH_PROVIDERS = new Set(['openai-subscription']);

/** Pick a sensible default model from a discovered list — prefer the latest
 *  Claude, else the first entry (docs/mac-app.md §5). */
export function pickDefaultModel(models: ModelInfo[]): string | undefined {
  const claude = models.find((m) => /claude/i.test(m.id));
  return (claude ?? models[0])?.id;
}
