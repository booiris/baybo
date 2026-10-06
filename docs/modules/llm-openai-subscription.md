# llm-openai-subscription — Sign in with ChatGPT

## Goal

Let users connect Baybo to their ChatGPT plan through OpenAI's official
dynamic registration and token-sharing flow. Baybo runs its own agent loop
and calls the public Responses API; it does not require a Codex binary.

Provider ID remains `openai-subscription`. All entries in one workspace
share one active credential. External `codex exec` authentication is separate.

Sources:
[registration and sign-in](https://developers.openai.com/siwc/token-sharing-open-source/sign-in),
[models and inference](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference),
[accounts and sessions](https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions),
[errors and recovery](https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery).

Open-source tools and personal local projects can dynamically register during
sign-in. Paid or remotely hosted applications must request access before
offering the integration; open-source distribution alone does not establish
eligibility for every deployment. Independent authorization does not create
an additional plan allowance.

## Authentication

First sign-in sends these parameters to
`https://auth.openai.com/api/accounts/authorize`:

- `client_id=dynamic_agent_client`
- `agent_name_hint=Baybo`
- Persistent installation `ext_agent_host_id=urn:uuid:...`
- `scope=openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`
- `resource=https://api.openai.com/v1`
- Fresh random state, nonce and S256 PKCE challenge
- An HTTP loopback redirect on `127.0.0.1`, path `/auth/callback`

The listener binds before displaying the URL. It prefers port 1455 and uses
an available port if that port is occupied. The entire callback wait is bounded
to five minutes; reads and writes on individual connections are bounded too.
The shared setup/CLI login picker offers automatic callback on this computer
or manual callback paste for SSH / another computer. In manual mode, open the
authorization link, approve Baybo, copy the complete final callback URL from
the browser address bar (even if the page cannot connect), and paste it into
the hidden terminal prompt. No SSH tunnel is needed. Manual submission checks
the exact redirect scheme, host, port and path, rejects userinfo/fragments,
then uses the same state/client validation and PKCE exchange as the listener.
The five-minute deadline is checked after terminal input; expired submissions
are rejected before code exchange. Cancellation or failure keeps existing credentials.
There is no device-code login for this flow.

Validate callback path and state, reject duplicate parameters and authorization
errors, and require an issued client ID for new registration. Returning sign-in
may omit the client ID, but cannot replace it with a different one. Save the
issued registration before exchanging the code, so an expired code does not
create another application connection on retry.

Exchange the code at `https://auth.openai.com/api/accounts/oauth/token`, using
the issued client ID, original verifier, exact redirect URI and resource.
No client secret or manually requested client ID is required by the dynamic
registration protocol.

Verify the ID token signature using OpenAI's published JWKS, resolved through
OpenID discovery. Enforce RS256, issuer, issued-client audience, expiration,
nonempty subject and the pending nonce. Returning sign-in must preserve the
registration's verified subject. A failed attempt keeps the active credentials.

The token response's actual granted scopes decide whether plan usage is
enabled. Identity-only connections are retained, but model discovery and
inference are blocked. Reauthentication of a connection without plan permission
requests consent using `prompt=consent`; ordinary reconnections do not force it.

## Durable state

Encrypted vault entries:

| Key | Contents |
| --- | --- |
| `llm.openai-subscription.host` | Persistent host UUID |
| `llm.openai-subscription.registration` | Issued client ID and verified subject, retained through logout |
| `llm.openai-subscription.tokens` | Active access, refresh and ID tokens, expiry, granted scopes and verified client/subject connection |

The host and registration are separate from tokens so logout and a permanently
failed refresh retain the information needed to reconnect. This version keeps
the existing single-registration surface; multiple saved accounts and an account
picker remain future work.

New access tokens may be opaque. Expiry comes from the token endpoint's
`expires_in`, rather than decoding an assumed access-token JWT. The encrypted
token bundle replaces all rotated credentials, expiry and scopes together.

## Refresh and logout

The existing per-credential `RefreshCoordinator` retains its in-process
single-flight gate, cross-process advisory lock, proactive background refresh,
preflight expiry check and one refresh/retry after HTTP 401.

Refresh uses the issued client ID associated with the freshest complete
credential bundle, including after adopting another process's rotation.
It sends the refresh grant and resource to the official token endpoint.
An omitted replacement refresh or ID token retains the previous value.
An omitted scope retains the previous grant; an explicit scope replaces it.
A replacement ID token must verify and retain the account subject.

Classify terminal refresh errors by machine-readable OAuth code:
`invalid_grant`, `invalid_refresh_token`, `token_expired`,
`refresh_token_expired`, `refresh_token_invalidated`,
`refresh_token_reused`. Those clear unusable tokens, retaining registration.
Network/infrastructure errors and `invalid_client` preserve credentials.

Logout resolves `revocation_endpoint` from OpenID discovery and sends the
refresh token and issued client ID. Local token clearing remains best effort
in the CLI removal flow, with remote revocation success reported separately.
A failed remote revoke is reported to the user.

## Inference and model discovery

New connections use:

- `GET https://api.openai.com/v1/models`
- `POST https://api.openai.com/v1/responses`
- `Authorization: Bearer <access_token>`
- `store=false`, `stream=true`

Discovery preserves server order and only exposes rows with
`visibility=list`. It never adds the legacy static model supplement.
The setup wizard seeds a lite model only when the actual catalog lists it.

The existing rig-to-Responses conversion handles system instructions, messages,
images, PDFs, function calls/results, reasoning and prompt cache keys.
New connections do not send Codex originator, account, experimental or cache
affinity headers.

Inference succeeds only after a valid `response.completed` event.
Interrupted streams, `response.failed`, `response.incomplete` and explicit
errors surface failures. Both LF and CRLF SSE framing are supported.
`subscription_sharing_usage_limit_exceeded` becomes `QuotaExhausted`;
`subscription_sharing_usage_unavailable` remains transient.
Usage errors link to [ChatGPT Settings → Usage](https://chatgpt.com/settings/usage).

Bearer destination validation adds `api.openai.com` to the existing OpenAI
allowlist and still requires HTTPS. The existing environment-only
`BAYBO_OPENAI_SUBSCRIPTION_UNSAFE_BASE_URL` override remains for deliberate
custom endpoints.

## Legacy migration

A token bundle without `connection` is an existing Codex credential.
It continues to use the legacy backend, refresh client ID and headers.
Compatibility is isolated in `legacy_oauth.rs`; no new login uses that client.

The public default base URL is now `https://api.openai.com/v1`.
An explicitly configured former default is normalized so routing follows the
credential: legacy tokens use the old backend, new tokens use the public API.
Other custom endpoints retain their configured destination.

Run `baybo llm edit` and select `OAuth login (re-authenticate)` to migrate.
The user must authorize Baybo in the browser; existing CLI tokens are never
relabeled as a Baybo connection.

## CLI and module boundaries

`baybo setup` / `baybo llm add` invoke browser PKCE when selecting this provider.
`baybo llm edit` reconnects the retained registration.
`baybo llm remove` revokes and clears tokens when removing the final entry.

`baybo-llm` owns OAuth, JWT validation, vault access, token refresh,
Responses conversion, streaming and model discovery. CLI and setup only
present the authorization URL and render diagnostics. OAuth persists a validated login under the refresh locks and replaces the shared cache immediately, so a running gateway cannot keep the old credential solely because its token expires later.

| Module | Responsibility |
| --- | --- |
| `oauth.rs` | Dynamic registration, callback, verified identity, refresh and revocation |
| `legacy_oauth.rs` | Refresh/revoke for pre-migration credentials |
| `token_bundle.rs` | Durable credential shape, permission gate, legacy display parsing |
| `token_store.rs` | Vault persistence and credential identity |
| `refresh_coordinator.rs` | Shared cache and process/peer refresh coordination |
| `completion_model.rs` | Responses conversion, routes, model catalog, SSE |
| `catalog.rs` | Legacy catalog supplement and optional lite-model seed |
| `factory.rs` | Provider construction and bearer destination policy |

## Verification

Tests cover dynamic and returning authorization parameters, callback state and
client binding, ID token signature/issuer/audience/expiry/nonce checks,
opaque access-token expiry, granted scopes, refresh rotation and error
classification, public model/Responses requests, permission enforcement,
terminal streaming events, and retained legacy refresh coordination.

Identity validation tests generate an ephemeral RSA key and matching JWKS in
memory. No test private key is stored in the repository.

Live browser authorization, account eligibility and plan usage require a real
user-authorized smoke test. They are not established by offline tests.
