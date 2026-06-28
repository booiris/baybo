// Typed admin-API client built on top of the generated OpenAPI schema.
//
// The Rust side emits `docs/openapi.json` via the `openapi_spec_sync`
// test; `npm run gen:api` rewrites `./schema.d.ts` from it. Handlers and
// response DTOs therefore drive the types here, so adding/renaming a
// route or DTO field fails `tsc` until the frontend catches up.
//
// Usage:
//
//   const api = createAdminClient({ baseUrl: '/', token });
//   const { data, error } = await api.GET('/v1/status');

import createClient, { type Client } from 'openapi-fetch';

import type { paths } from './schema';

export interface AdminClientOptions {
  /** Admin listener base URL. `/` when served by the gateway itself. */
  baseUrl: string;
  /** Bearer token for the admin TCP listener. */
  token: string;
}

export type AdminClient = Client<paths>;

export function createAdminClient(opts: AdminClientOptions): AdminClient {
  return createClient<paths>({
    baseUrl: opts.baseUrl,
    headers: { Authorization: `Bearer ${opts.token}` },
  });
}
