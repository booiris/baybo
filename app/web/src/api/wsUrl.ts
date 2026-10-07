// WebSocket URL for an admin-listener endpoint. Same origin as the admin
// listener in production; the dev Vite proxy rewrites /v1 (including the WS
// upgrade) to the gateway.
export function buildWsUrl(baseUrl: string, path: string, token: string): string {
  const u = new URL(baseUrl);
  u.protocol = u.protocol === 'https:' ? 'wss:' : 'ws:';
  u.pathname = path;
  u.search = '';
  // Browser WebSocket cannot set Authorization, so the admin auth
  // middleware accepts this query-param form and strips it before tracing.
  u.searchParams.set('token', token);
  return u.toString();
}
