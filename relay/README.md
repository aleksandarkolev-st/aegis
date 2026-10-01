# Aegis cloud relay

This standalone Rust service connects messaging channels to Aegis through a channel-neutral `MessagingChannel` contract. The relay core owns identity, pairing, routing, notification delivery, and receipts. Evolution is the only live adapter today and implements that contract for WhatsApp. The metadata schema also reserves an iMessage channel identity, but no iMessage adapter is implemented or tested here; it remains an optional macOS-only integration.

The relay does not run code or make tool decisions. A channel adapter normalizes inbound text into a small typed command on a per-installation JetStream subject. Aegis connects outbound to NATS over TLS, consumes only its own command subject, and publishes its allowlisted remote events. The active channel adapter renders those events for destinations paired to the same actor.

## Stored data

PostgreSQL contains only six metadata tables: users, installations, channel_bindings, pairing_tokens, delivery_receipts, and notification_preferences. Channel identity is stored independently of provider names, so a future adapter can reuse relay routing and delivery.

- Pairing codes are random 256-bit values. PostgreSQL stores their SHA-256 hashes and 5-minute expiry; redemption is atomic and one time.
- Each installation belongs to one relay user and can register multiple actor IDs. Pairing metadata keeps one current token row per actor; actor IDs are unique across installations and cannot be moved to another installation.
- Inbound webhook receipts keep only channel, sender ID, provider message ID, target installation/binding, disposition, and status for 30 days. They prevent provider retries from publishing commands twice or retrying a consumed pairing code.
- Outbound receipts keep channel, event/binding IDs, status, attempt count, provider message ID, and timestamps for 90 days.
- Missing notification preferences enable state changes (task_started, approval_required, blocked, completed, failed, and reply) and suppress routine progress. Progress can be explicitly enabled per channel with the admin preference endpoint.
- No message text, repository data, model context, provider credential, shell output, or event transcript is written to PostgreSQL or application logs.
- JetStream command and event streams use file storage, WorkQueue retention, and a 64 MiB cap with an explicit discard-oldest policy. Commands have a 15-minute maximum age; events have a 24-hour maximum age. Acknowledged items are removed from the work queue. Pending payloads, including normalized command text and reply text, remain only in this bounded NATS queue until acknowledgement or expiry; they are not copied into PostgreSQL or application logs. The relay updates existing stream configuration at startup, so an existing event stream receives the 24-hour policy too. An event can be retained for up to one day while the relay is offline if NATS remains available and the stream stays below 64 MiB; reaching the byte cap can evict older events sooner.

The AegisEvent schema accepts only task_started, progress, approval_required, blocked, completed, failed, and reply. Every event must include a valid actor_id. Unknown fields such as shell output or repository context are rejected. Approval events include a display_detail of at most 512 bytes with only the capability and bounded target, plus the challenge expiry; these are never persisted or logged. Expired approval notifications are acknowledged and dropped. reply_text is accepted only for a reply event and is capped at 4 KiB.

## Remote commands

Only direct inbound user text is accepted. The provider adapter rejects groups, messages sent by the relay's WhatsApp account, status events, media-only messages, malformed sender IDs, and unauthenticated webhooks. Message text is line-ending normalized and capped at 8 KiB. It is never logged.

An unpaired number can redeem a one-time code with either exact form: `/pair <code>` or `AEGIS <code>`. The command name is case-insensitive for the `AEGIS` form; the code must be one exact 43-character token with no trailing text. Pairing codes are consumed once. These prefixes are reserved for pairing and are not forwarded as ordinary messages.

| WhatsApp text | Typed command |
| --- | --- |
| Ordinary text or /message <text> | message |
| /tasks or /list_tasks | list_tasks |
| /status | status |
| /result [task-id] | result |
| /evidence [task-id] | evidence |
| /details [task-id] | details |
| /pause | pause |
| /resume | resume |
| /cancel [task-id] | cancel |
| /select_task <task-id> or /use <task-id> | select_task |
| /approve_once <challenge-id> | approve_once |
| /deny <challenge-id> | deny |

`/result [alias]` is available only after a task reaches a terminal state. It returns the saved final summary, bounded to 2 KiB and marked as verified, unverified, or not verified based on the recorded task state. `/evidence [alias]` returns at most eight receipts that are referenced by verified explicit obligations and belong to the current workspace revision. Each row contains only a safe capability name, a 12-character hash prefix, and byte count; artifact contents, paths, command arguments, stdout, and previews are never returned. Conversational answers have no evidence receipts.

Approval commands carry the challenge ID. There is no /approve alias. IDs are restricted to 128 ASCII letters, digits, underscores, hyphens, or periods.

Each command envelope includes installation_id, actor_id, stable envelope_id and request_id, issued/expiry timestamps, source channel/sender/message IDs, and the typed command. Commands expire after five minutes. Retries reuse stable IDs and set JetStream's Nats-Msg-Id. Event envelopes include installation_id, actor_id, challenge_id, display_detail, and reply_text; nullable event-specific fields are null when unused. The relay restricts fan-out to the matching paired destination.

Relay-to-Aegis authentication uses TLS and separate NATS users with subject permissions. The relay authenticates as `aegis-relay`; each device authenticates as `aegis-<canonical lowercase installation UUID>` with its own password. Aegis validates the installation, expiry, typed command, and envelope actor_id against its local actor-to-run mapping before dispatch. No actor HMAC key is sent to or shared with the relay. Each Aegis event carries actor_id from the local actor-to-run scope; before delivery, the relay selects only active bindings for the same installation, actor, and active channel adapter. A phone paired to another actor on the same installation cannot receive that event.

The relay's `relay-whatsapp-delivery` durable pull consumer is supervised independently from HTTP serving. A JetStream pull error or end-of-stream drops the current pull stream, waits with exponential backoff from 1 to 30 seconds, then gets or creates the same durable consumer and resumes pending events. The backoff resets after a consumer stays connected for at least a minute. Unacknowledged events remain eligible for redelivery until acknowledged or removed by the event stream's one-day/64 MiB limits.

## NATS principal provisioning

Provision NATS users separately from HTTP channel pairing. The admin pairing route does not issue NATS credentials. Use a current supported NATS server with the named, single-filter consumer create API (introduced in 2.9). Replace any server-wide `authorization.token` configuration with an explicit user list. Generate a distinct random password for the relay and for every installation, and store production server passwords as bcrypt hashes or use an equivalent external provisioning system. Retain TLS. Rotate or revoke an installation's NATS user independently of channel pairing when its device credential is compromised.

The legacy names `NATS_AUTH_TOKEN` and `--nats-token-env` now identify **user passwords**, not NATS token authentication. The relay reads its password from `NATS_AUTH_TOKEN`. Aegis stores only the environment variable name in `remote.json`; its daemon reads the installation password from that variable. Neither client accepts a username override or falls back to shared token authentication.

For installation `I`, substitute its canonical lowercase hyphenated UUID and set `C` to `aegis-I`. Its NATS user is `C`. Give it precisely these publish permissions:

```text
aegis.events.I
$JS.API.CONSUMER.INFO.AEGIS_COMMANDS.C
$JS.API.CONSUMER.CREATE.AEGIS_COMMANDS.C.aegis.commands.I
$JS.API.CONSUMER.MSG.NEXT.AEGIS_COMMANDS.C
$JS.ACK.AEGIS_COMMANDS.C.*.*.*.*.*
```

Give it only `_INBOX.aegis.device.I.>` as a subscribe permission. Aegis sets `_INBOX.aegis.device.I` as its custom inbox prefix. Pull messages and API/publish replies arrive through those inboxes; no direct subscription to the command subject or stream-info permission is needed. Before pulling, the daemon checks the returned consumer's stream, name, durable name, exact command filter, pull mode, and explicit acknowledgement policy. An existing consumer with different authority fails closed.

Give `aegis-relay` precisely these publish permissions:

```text
aegis.commands.*
$JS.API.STREAM.CREATE.AEGIS_COMMANDS
$JS.API.STREAM.UPDATE.AEGIS_COMMANDS
$JS.API.STREAM.CREATE.AEGIS_EVENTS
$JS.API.STREAM.UPDATE.AEGIS_EVENTS
$JS.API.STREAM.INFO.AEGIS_EVENTS
$JS.API.CONSUMER.INFO.AEGIS_EVENTS.relay-whatsapp-delivery
$JS.API.CONSUMER.CREATE.AEGIS_EVENTS.relay-whatsapp-delivery.aegis.events.*
$JS.API.CONSUMER.MSG.NEXT.AEGIS_EVENTS.relay-whatsapp-delivery
$JS.ACK.AEGIS_EVENTS.relay-whatsapp-delivery.*.*.*.*.*
```

Give the relay only `_INBOX.aegis.relay.>` as a subscribe permission; its custom inbox prefix is `_INBOX.aegis.relay`. The relay also checks the returned durable consumer's scope and acknowledgement policy before pulling. Allow lists deny subjects outside the list. Do not add unrestricted users, broad `$JS.API.>` or `$JS.ACK.>` grants, shared `_INBOX.>` subscriptions, `allow_responses`, unfiltered `CONSUMER.CREATE` endpoints, or legacy `CONSUMER.DURABLE.CREATE` endpoints. In particular, a device grant for a consumer name without the exact filter suffix would let it request a different filter in the payload. The filtered endpoint checks the payload against the filter in its subject and rejects multiple filters. See the [NATS consumer API implementation](https://github.com/nats-io/nats-server/blob/v2.12.0/server/jetstream_api.go#L4286) and [authentication documentation](https://docs.nats.io/learn/security/authentication-basics).

These ACK grants cover the standalone server configuration here. If deploying JetStream domains or cross-account routing that changes ACK subjects, scope the additional emitted ACK format to the same stream and consumer, and configure the matching API prefix as required by that topology. Do not broaden an ACK grant to compensate for a topology mismatch. Administrative inspection, cleanup, and credential provisioning should use a separate trusted operator principal; devices and the relay are not granted consumer-delete permissions.

The shared `AEGIS_COMMANDS` and `AEGIS_EVENTS` streams isolate message access through these permissions, but **do not isolate availability or storage quotas**. One device's event flood can fill the shared 64 MiB event stream and evict another device's pending events. Deploy per-installation streams with exact subjects and separate limits, or per-installation JetStream accounts, when that boundary is required; those deployments also need corresponding relay consumer lifecycle changes.

## WhatsApp delivery guarantee

Outbound WhatsApp delivery is **at-least-once**, not exactly-once. Aegis event IDs and per-destination PostgreSQL receipts suppress retries after the relay commits a successful delivery receipt. The current Evolution `sendText` DTO has no provider idempotency-key field ([controller](https://github.com/evolution-foundation/evolution-api/blob/main/src/api/controllers/sendMessage.controller.ts), [DTO](https://github.com/evolution-foundation/evolution-api/blob/main/src/api/dto/sendMessage.dto.ts)), so a send that Evolution accepts before the relay receives its response or commits the receipt is ambiguous. Retrying that event can send a duplicate WhatsApp message. The relay passes stable event IDs to the provider interface for adapters that support idempotency, but Evolution cannot use them for provider-side deduplication.

## HTTP routes

- GET /healthz returns ok.
- POST /admin/v1/installations requires Authorization: Bearer RELAY_ADMIN_TOKEN. JSON body:

      {"actor_id":"local-actor-id","installation_id":"<optional canonical Aegis UUID>"}

  The relay creates the user and installation if needed, then returns a 5-minute pairing code once. Under the trusted admin bearer principal, an existing installation UUID identifies that installation's user; additional actor IDs can be provisioned under the same installation. Re-provisioning the same installation/actor pair rotates its one-time code and keeps the user, installation, and actor binding scope. An actor ID already owned by another installation is rejected with 409. The relay admin bearer is the trusted bootstrap authority for this single deployment; it is not an end-user credential.
- POST /admin/v1/installations/{installation_id}/notifications/{event_kind} requires the admin bearer token and JSON {"enabled":false} (or true); this compatibility route changes WhatsApp preferences.
- POST /admin/v1/installations/{installation_id}/channels/{channel}/notifications/{event_kind} changes preferences for `whatsapp` or the reserved `imessage` identity.
- POST /admin/v1/installations/{installation_id}/bindings/revoke requires the admin bearer token and JSON {"actor_id":"local-actor-id","sender_id":"+4915112345678"}. It deactivates only the WhatsApp binding matching those values and consumes any unused pairing code for that actor on the installation.
- POST /admin/v1/installations/{installation_id}/channels/{channel}/bindings/revoke applies the same sender-specific operation to one channel.
- POST /admin/v1/installations/{installation_id}/actors/{actor_id}/bindings/revoke requires the admin bearer token and no request body. It idempotently consumes any unused pairing code and deactivates every channel binding for exactly that actor and installation. It returns 204 even if nothing is currently paired, so the local daemon can use it as best-effort cloud cleanup after disabling the actor locally.
- POST /v1/webhooks/whatsapp/evolution authenticates before interpreting the message JSON. It accepts either a dedicated x-aegis-webhook-token header, or an HMAC signature in x-aegis-timestamp and x-aegis-signature. The signature is v1= plus base64url-no-pad HMAC-SHA256 over timestamp, a period, and the raw body; it expires after five minutes. Use a dedicated webhook secret separate from the Evolution API key.

Configure Evolution's webhook to send x-aegis-webhook-token. If it cannot set custom webhook headers, place a trusted HTTPS ingress adapter in front that validates provider authentication and adds this dedicated header. Do not expose the webhook route without authentication.

## Environment

Required variables:

| Variable | Purpose |
| --- | --- |
| DATABASE_URL | PostgreSQL URL. Production should use sslmode=verify-full. |
| NATS_URL | tls://host:port; plaintext NATS is rejected. |
| NATS_AUTH_TOKEN | Password for the fixed `aegis-relay` NATS user, at least 32 characters. Keep it in a secret manager. |
| NATS_TLS_ROOT_CERT | Optional PEM CA path for a private NATS certificate authority. |
| RELAY_ADMIN_TOKEN | Admin API bearer secret, at least 32 characters. |
| EVOLUTION_BASE_URL | HTTPS Evolution API base URL. HTTP is allowed only for localhost development. |
| EVOLUTION_INSTANCE | Evolution instance name. |
| EVOLUTION_API_KEY | Evolution outbound API credential; held in process configuration only. |
| EVOLUTION_WEBHOOK_SECRET | Dedicated webhook token/HMAC secret, at least 32 characters. |
| RELAY_BIND_ADDR | Defaults to 127.0.0.1:8787. |

In production, bind the process to a private interface and expose the webhook through an HTTPS reverse proxy with request limits and authentication. Do not expose a plaintext or unauthenticated NATS listener. Database initialization creates only the six tables above.

## Local services and smoke test

Local Compose binds PostgreSQL and NATS to loopback only. NATS uses TLS, the relay user, and two devices with exact installation permissions; its test certificate is generated locally and is not committed. PostgreSQL plaintext is allowed only with RELAY_ALLOW_INSECURE_LOCAL_DATABASE=true and a localhost or Compose-service database host.

Compose requires `NATS_AUTH_TOKEN` for the relay password and the following complete values for each device. Use prefix `AEGIS_NATS_DEVICE_` for the first device and `AEGIS_NATS_DEVICE2_` for the second; substitute `I` and `C` as described above. NATS config variables do not interpolate UUIDs inside subject strings, so provide each full subject rather than a UUID placeholder. There are no shared-token or wildcard-device defaults.

| Variable suffix | Value |
| --- | --- |
| USERNAME | `aegis-I` |
| PASSWORD | Unique device password, separate from `NATS_AUTH_TOKEN` |
| EVENT_SUBJECT | `aegis.events.I` |
| CONSUMER_INFO | `$JS.API.CONSUMER.INFO.AEGIS_COMMANDS.C` |
| CONSUMER_CREATE | `$JS.API.CONSUMER.CREATE.AEGIS_COMMANDS.C.aegis.commands.I` |
| CONSUMER_NEXT | `$JS.API.CONSUMER.MSG.NEXT.AEGIS_COMMANDS.C` |
| ACK | `$JS.ACK.AEGIS_COMMANDS.C.*.*.*.*.*` |
| INBOX | `_INBOX.aegis.device.I.>` |

Derive these values from the installation identity before starting NATS. Supply the first device's password to its daemon under the environment variable named by `--nats-token-env`. Treat the inputs as operator configuration: setting a wildcard where an exact subject is specified would weaken the boundary.

For the four `$JS...` values, include double quote characters around the full value in the environment variable (for example, `"$JS.API.CONSUMER.INFO.AEGIS_COMMANDS.C"`). NATS parses environment variable values as config text; these quotes keep `$JS` literal, and are removed before the subject permission is applied.

From the repository root in PowerShell, run:

    .\relay\dev\start-local.ps1

It starts Postgres and NATS, exports development-only settings, and runs the relay. The local webhook listens only on 127.0.0.1:8787; external webhook traffic still needs HTTPS.

The fake-adapter smoke tests need no provider account, database, or NATS. They exercise the channel-neutral relay core and local Aegis authority with fixtures; they do not represent a live iMessage adapter:

    cargo test --manifest-path relay/Cargo.toml --test local_smoke

Or run .\relay\scripts\smoke.ps1. Provider parsing and webhook authentication have fixture-based unit tests.

### Pair an Aegis installation

After the relay is running and Aegis has a locally saved provider/model profile, set the local development credentials in a separate PowerShell session and pair the installation:

    $env:AEGIS_RELAY_ADMIN_TOKEN = "local-development-admin-token-not-for-production"
    $env:AEGIS_NATS_TOKEN = $env:AEGIS_NATS_DEVICE_PASSWORD
    aegis remote pair --relay-admin-url http://127.0.0.1:8787 --admin-token-env AEGIS_RELAY_ADMIN_TOKEN --nats-url tls://localhost:4422 --nats-token-env AEGIS_NATS_TOKEN --nats-root-cert .\relay\dev\certs\nats.crt

Send the one-time `/pair <code>` command printed by Aegis from the WhatsApp number to bind. Then check `aegis remote status` and keep `aegis remote run` running while you use the relay. Pairing stores no credential values; Aegis reads them from the named environment variables. Supply the device password in this session before running the daemon. These sample credentials are for local development only. Configure Evolution with a valid account, HTTPS endpoint, API key, and webhook authentication before expecting WhatsApp messages to reach the local relay.

To exercise PostgreSQL pairing rotation, actor-scoped binding lookup, and durable inbound dedupe against the local database, run .\relay\scripts\db-smoke.ps1. It starts only the loopback-bound Postgres service and runs the optional integration test.

For the isolated cross-process end-to-end check, run .\relay\scripts\e2e-smoke.ps1. It builds the real Aegis binary, starts a temporary PostgreSQL and TLS JetStream Compose project on loopback ports, exercises the Aegis pairing CLI and remote daemon through the relay HTTP router, and tears down only that uniquely named test project and its volumes. The fake WhatsApp adapter avoids provider credentials; this verifies the local relay path, not delivery through a live Evolution account.
