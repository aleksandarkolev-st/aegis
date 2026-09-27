# Direct provider transition

The user requires Aegis to own its agent loop and use ChatGPT/Grok login without starting Codex or Grok Build. A smaller nested-CLI prompt is not the requested solution. Claude is pending at the user's request; custom OpenAI-compatible HTTP support remains required.

## Current state

`src/direct.rs` implements a direct transport core, not a finished login integration. ChatGPT uses Responses SSE; Grok uses chat completions. Requests carry only Aegis instructions, bounded state and the existing six-action schema. No subprocess, native tool registry, collaboration prompt, skills loader or automatic CLI fallback is used by this module. Production provider destinations are fixed, not taken from workspace configuration.

Credential objects have no Debug or serialization implementation. Explicit saved-session reads are bounded to 1 MiB, regular-file checked, provider/account bound and read-only. They do not copy refresh tokens, mutate native credentials, select an arbitrary Grok account, or silently substitute an API key. They are not a replacement for Aegis-owned browser login, secure storage or refresh.

ChatGPT requires an explicit selected account. Grok's source-defined `X-XAI-Token-Auth` value selects its user-token authentication route; Aegis sends its own client identifier and User-Agent, not an official CLI User-Agent. HTTP 403 is a refusal, not an invitation to spoof another client or bypass access controls. Source-visible protocols do not guarantee third-party entitlement or stable hosted-service compatibility.

Only completed responses are accepted. Truncated/failed/refused responses, duplicate completions and unexpected native tool calls fail closed. Credentials echoed into assistant text are redacted before action parsing. Reported input/output/cache usage is preserved without subtracting bootstrap or cached tokens; missing usage stays unknown, never a zero-cost claim or character-based estimate. Redirects are disabled; connection, body size, interruption and timeout checks precede acceptance.

## Evidence and remaining work

- Seven direct transport/binding checks and the 127-test Windows library suite passed after correcting a fixture socket race. Ten Node checks passed. Logs: `.arun/direct-transport-functional-20260927-session-binding.log`, `.arun/direct-transport-library-20260927-corrected.log`, `.arun/npm-direct-transport-20260927.log`.
- The first complete library run failed two HTTP fixtures because accepted sockets inherited nonblocking mode on Windows. The fixture now explicitly returns them to blocking mode; no timeout/assertion was weakened. Original log: `.arun/direct-transport-library-20260927.log`.
- Normal model dispatch, F4 sign-in, provider setup and catalog discovery still use the old adapters until replaced. The user-global installation is unchanged. These tests do not prove live direct authentication, installed operation, lower billed cost or benchmark superiority.
- Next: Aegis-owned sign-in/secure refresh, explicit saved-session connection, direct model/catalog dispatch, removal of automatic provider CLI installation/execution, conversion of legacy launcher fixtures, full Windows/Linux packaging and installed self-use. No new coding or ARC-AGI-3 evaluation starts before this readiness gate.

## Reviewed protocols

Codex source revision `41f9084b30812db321a0b592def4f500d1e79cf4`: [Responses request shape](https://github.com/openai/codex/blob/41f9084b30812db321a0b592def4f500d1e79cf4/codex-rs/codex-api/src/common.rs), [provider destination](https://github.com/openai/codex/blob/41f9084b30812db321a0b592def4f500d1e79cf4/codex-rs/model-provider-info/src/lib.rs), [login token structure](https://github.com/openai/codex/blob/41f9084b30812db321a0b592def4f500d1e79cf4/codex-rs/login/src/token_data.rs). [Official authentication documentation](https://learn.chatgpt.com/docs/auth) describes cached login storage and security, but is not a blanket certification of this independent client.

Grok Build source revision `f0e3be1100ef5252488e3be8bb0e91cf68d8c305`: `crates/codegen/xai-grok-shell/src/agent/config.rs`, `xai-grok-sampler/src/client.rs`, `xai-grok-login/src/config.rs`, `model.rs`, `storage.rs`, and `grok_auth_credentials.rs`. These identify its direct HTTP destination, OAuth scope binding, request encoding and token-auth route. No upstream agent code or prompts were copied.

[Anthropic's current third-party authentication guidance](https://support.claude.com/en/articles/13189465-log-in-to-your-claude-account) directs developers to API authentication and prohibits identity misrepresentation or routing third-party traffic against subscription limits. Claude remains pending rather than masquerading as Claude Code.
