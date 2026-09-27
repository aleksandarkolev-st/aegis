# Authentication source audit

Reviewed 2026-09-27. Read upstream source only; no native agent was launched, installed, built or modified. This audit does not establish permission to use a provider's registered OAuth client in a third-party application, subscription entitlement, or successful fresh Aegis authentication.

## Pinned sources

- Codex: `32f578485143354d1c321840a3e990aabdbaca9c`, fetched from [openai/codex](https://github.com/openai/codex/tree/32f578485143354d1c321840a3e990aabdbaca9c). The local audit clone's worktree is older; current code was read through `git show FETCH_HEAD:<path>` and GitHub's revision-pinned raw files.
- Grok Build: `f0e3be1100ef5252488e3be8bb0e91cf68d8c305`, read from [xai-org/grok-build](https://github.com/xai-org/grok-build/tree/f0e3be1100ef5252488e3be8bb0e91cf68d8c305). Its audit worktree is clean.
- [Official OpenAI authentication documentation](https://learn.chatgpt.com/docs/auth) is separate from implementation evidence; upstream implementation is not a promise that third-party subscription access is supported.

## Codex

[Browser server](https://github.com/openai/codex/blob/32f578485143354d1c321840a3e990aabdbaca9c/codex-rs/login/src/server.rs): browser authorization code with PKCE and state, issuer `auth.openai.com`, loopback `/auth/callback`, registered ports 1455 and 1457. Authorization includes organization metadata and optional workspace constraints. The current scope also requests connector read/invoke permissions; Aegis must not inherit unrelated capability grants merely to imitate an agent.

[Device flow](https://github.com/openai/codex/blob/32f578485143354d1c321840a3e990aabdbaca9c/codex-rs/login/src/device_code_auth.rs): JSON user-code request to `/api/accounts/deviceauth/usercode`; JSON polling at `/api/accounts/deviceauth/token`; returned authorization code/verifier exchanged at `/oauth/token`, using the device callback. Polling treats HTTP 403/404 as pending and uses a local 15-minute wait. This is not the generic OAuth device grant used by Grok.

[Auth manager](https://github.com/openai/codex/blob/32f578485143354d1c321840a3e990aabdbaca9c/codex-rs/login/src/auth/manager.rs) distinguishes expired, reused, revoked and account-mismatched refresh credentials. Browser token exchange guards retry decisions against spending a one-time authorization code twice. Aegis should preserve these invariants, not retry every authentication failure or fall back to another account.

## Grok Build

[Browser login](https://github.com/xai-org/grok-build/blob/f0e3be1100ef5252488e3be8bb0e91cf68d8c305/crates/codegen/xai-grok-login/src/oidc/login.rs) performs discovery, PKCE, state and nonce generation, binds an OS-assigned loopback port in production and validates the chosen principal. [Protocol implementation](https://github.com/xai-org/grok-build/blob/f0e3be1100ef5252488e3be8bb0e91cf68d8c305/crates/codegen/xai-grok-login/src/oidc/protocol.rs) uses S256 and form authorization-code exchange. The browser login path has a manual bare-code fallback that skips state comparison when no state is supplied; Aegis's future callback flow must require state rather than copying that relaxation.

[Device code](https://github.com/xai-org/grok-build/blob/f0e3be1100ef5252488e3be8bb0e91cf68d8c305/crates/codegen/xai-grok-login/src/device_code.rs) posts to `/oauth2/device/code`, then `/oauth2/token` with `urn:ietf:params:oauth:grant-type:device_code`. It sleeps before polling, increases the interval by five seconds for `slow_down`, and handles denial/expiry separately. Its server expiry is not restricted to 900 seconds; native polling instead floors the wait at ten minutes. Aegis should not copy that floor: a short server expiry must remain short, while a longer server expiry can coexist with a shorter local waiting limit.

[Default scopes](https://github.com/xai-org/grok-build/blob/f0e3be1100ef5252488e3be8bb0e91cf68d8c305/crates/codegen/xai-grok-login/src/config.rs) include remote conversations/workspaces read and write. Aegis currently asks only for identity, offline access and inference-proxy access. [Refresh chain](https://github.com/xai-org/grok-build/blob/f0e3be1100ef5252488e3be8bb0e91cf68d8c305/crates/codegen/xai-grok-login/src/manager/refresh_chain.rs) locks, adopts sibling token rotations and rechecks before exchange; it explicitly protects refresh exchange from cancellation losing a rotated token. These protections already exist upstream; claiming Grok lacks refresh serialization would be incorrect.

## Aegis observations and implementation priorities

1. Aegis currently owns device authentication and direct inference/catalog requests. It does not launch a provider agent. Browser authorization-code/PKCE as the normal desktop choice is still missing, despite the existing menu saying browser sign-in. Keep device codes as an explicit headless/fallback choice.
2. Actual owned Grok sign-in failed before presenting a code: `Sign-in timing exceeds the supported bounds`. No browser approval or credential save occurred. The old validator conflated server expiry with its local 15-minute limit and capped polling intervals at 60 seconds. Aegis now accepts positive numeric server lifetimes but limits local waiting to `min(server lifetime, 15 minutes)`, without extending shorter server expiry. Positive intervals up to the local window are respected rather than clamped down. The UI says approve within that local window, not that the server expires the code then. Functional fixtures cover long lifetimes, integer extremes, short expiry and cancellation; the offending live field/value was not retained, so the exact live cause and a successful fresh sign-in remain unproven.
3. Read-only use of an existing ChatGPT access token returned an empty catalog with Aegis's package version; the reference catalog compatibility version returned seven selectable models. Own identity remains Aegis. This is catalog compatibility evidence, not fresh owned OAuth or installed-package usability proof. Existing Grok access was rejected with HTTP 401; never fall back or refresh someone else's saved credentials.
4. Keep Aegis sessions/catalogs private and account-generation-bound; never import upstream prompts, tools or routing configuration from model metadata. Cancellation must stop/join owned workers and preserve selected model/reasoning.
5. Audit refresh cancellation separately: a received rotated refresh credential must not be discarded just because the foreground UI cancels. Account/logout generation checks must prevent a completed exchange from resurrecting a signed-out session. Functional tests and live fresh approval are required before claiming this gate passed.
6. CLI sign-in failure currently returns a successful process status after displaying the error. Correct that without making cancellation or interactive back-navigation look like successful authentication.

Claude subscription authentication remains pending by user direction. No coding/ARC benchmark readiness, full plan completion or superiority over the upstream agents is asserted by this audit.
