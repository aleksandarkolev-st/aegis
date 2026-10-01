# Aegis cloud relay

This standalone Rust service bridges WhatsApp and Aegis. Evolution is the first WhatsApp adapter; the provider trait is the seam for a future Meta Cloud adapter.

The relay does not run code or make tool decisions. WhatsApp input becomes a small typed command on a per-installation JetStream subject. Aegis connects outbound to NATS over TLS, consumes only its own command subject, and publishes its allowlisted remote events. The relay renders those events as WhatsApp messages for paired destinations.

## Stored data

PostgreSQL contains only six metadata tables: users, installations, channel_bindings, pairing_tokens, delivery_receipts, and notification_preferences.

- Pairing codes are random 256-bit values. PostgreSQL stores their SHA-256 hashes and 10-minute expiry; redemption is atomic and one time.
- Each installation belongs to one relay user and can register multiple actor IDs. Pairing metadata keeps one current token row per actor; actor IDs are unique across installations and cannot be moved to another installation.
- Inbound webhook receipts keep only channel, sender ID, provider message ID, target installation/binding, disposition, and status for 30 days. They prevent provider retries from publishing commands twice or retrying a consumed pairing code.
- Outbound receipts keep event/binding IDs, status, attempt count, provider message ID, and timestamps for 90 days.
- No message text, repository data, model context, provider credential, shell output, or event transcript is written to PostgreSQL or application logs.
- JetStream command and event streams use file storage, WorkQueue retention, and a 64 MiB cap with an explicit discard-oldest policy. Commands have a 15-minute maximum age; events have a 24-hour maximum age. Acknowledged items are removed from the work queue. Pending payloads, including normalized command text and reply text, remain only in this bounded NATS queue until acknowledgement or expiry; they are not copied into PostgreSQL or application logs. The relay updates existing stream configuration at startup, so an existing event stream receives the 24-hour policy too. An event can be retained for up to one day while the relay is offline if NATS remains available and the stream stays below 64 MiB; reaching the byte cap can evict older events sooner.

The AegisEvent schema accepts only task_started, progress, approval_required, blocked, completed, failed, and reply. Every event must include a valid actor_id. Unknown fields such as shell output or repository context are rejected. Approval events include a display_detail of at most 512 bytes with only the capability and bounded target; it is never persisted or logged. reply_text is accepted only for a reply event and is capped at 4 KiB.

## Remote commands

Only direct inbound user text is accepted. The provider adapter rejects groups, messages sent by the relay's WhatsApp account, status events, media-only messages, malformed sender IDs, and unauthenticated webhooks. Message text is line-ending normalized and capped at 8 KiB. It is never logged.

| WhatsApp text | Typed command |
| --- | --- |
| Ordinary text or /message <text> | message |
| /tasks or /list_tasks | list_tasks |
| /status | status |
| /pause | pause |
| /resume | resume |
| /cancel [task-id] | cancel |
| /select_task <task-id> or /use <task-id> | select_task |
| /approve_once <challenge-id> | approve_once |
| /deny <challenge-id> | deny |

Approval commands carry the challenge ID. There is no /approve alias. IDs are restricted to 128 ASCII letters, digits, underscores, hyphens, or periods.

Each command envelope includes installation_id, actor_id, stable envelope_id and request_id, issued/expiry timestamps, source channel/sender/message IDs, and the typed command. Commands expire after five minutes. Retries reuse stable IDs and set JetStream's Nats-Msg-Id. Event envelopes include installation_id, actor_id, challenge_id, display_detail, and reply_text; nullable event-specific fields are null when unused. The relay restricts fan-out to the matching paired destination.

Relay-to-Aegis authentication uses TLS and NATS principal/subject ACLs. Give the relay service principal permission to publish `aegis.commands.*`, consume `aegis.events.*`, and create or inspect only the two named streams and the event-delivery consumer. Give each Aegis device permission to consume only `aegis.commands.<its-installation-uuid>` and publish only `aegis.events.<its-installation-uuid>`. Its NATS principal also needs the JetStream API requests and replies used to inspect the command stream, create or inspect its named durable pull consumer, pull messages, and acknowledge them; scope those to that stream and consumer instead of allowing all of `$JS.API.>` or `$JS.ACK.>`. Aegis validates the installation, expiry, typed command, and envelope actor_id against its local actor-to-run mapping before dispatch. No actor HMAC key is sent to or shared with the relay. Each Aegis event carries actor_id from the local actor-to-run scope; before delivery, the relay selects only active WhatsApp bindings with the same installation_id and actor_id. A phone paired to another actor on the same installation cannot receive that event.

The relay's `relay-whatsapp-delivery` durable pull consumer is supervised independently from HTTP serving. A JetStream pull error or end-of-stream drops the current pull stream, waits with exponential backoff from 1 to 30 seconds, then gets or creates the same durable consumer and resumes pending events. The backoff resets after a consumer stays connected for at least a minute. Unacknowledged events remain eligible for redelivery until acknowledged or removed by the event stream's one-day/64 MiB limits.

## WhatsApp delivery guarantee

Outbound WhatsApp delivery is **at-least-once**, not exactly-once. Aegis event IDs and per-destination PostgreSQL receipts suppress retries after the relay commits a successful delivery receipt. The current Evolution `sendText` DTO has no provider idempotency-key field ([controller](https://github.com/evolution-foundation/evolution-api/blob/main/src/api/controllers/sendMessage.controller.ts), [DTO](https://github.com/evolution-foundation/evolution-api/blob/main/src/api/dto/sendMessage.dto.ts)), so a send that Evolution accepts before the relay receives its response or commits the receipt is ambiguous. Retrying that event can send a duplicate WhatsApp message. The relay passes stable event IDs to the provider interface for adapters that support idempotency, but Evolution cannot use them for provider-side deduplication.

## HTTP routes

- GET /healthz returns ok.
- POST /admin/v1/installations requires Authorization: Bearer RELAY_ADMIN_TOKEN. JSON body:

      {"actor_id":"local-actor-id","installation_id":"<optional canonical Aegis UUID>"}

  The relay creates the user and installation if needed, then returns a 10-minute pairing code once. Under the trusted admin bearer principal, an existing installation UUID identifies that installation's user; additional actor IDs can be provisioned under the same installation. Re-provisioning the same installation/actor pair rotates its one-time code and keeps the user, installation, and actor binding scope. An actor ID already owned by another installation is rejected with 409. The relay admin bearer is the trusted bootstrap authority for this single deployment; it is not an end-user credential.
- POST /admin/v1/installations/{installation_id}/notifications/{event_kind} requires the admin bearer token and JSON {"enabled":false} (or true).
- POST /v1/webhooks/whatsapp/evolution authenticates before interpreting the message JSON. It accepts either a dedicated x-aegis-webhook-token header, or an HMAC signature in x-aegis-timestamp and x-aegis-signature. The signature is v1= plus base64url-no-pad HMAC-SHA256 over timestamp, a period, and the raw body; it expires after five minutes. Use a dedicated webhook secret separate from the Evolution API key.

Configure Evolution's webhook to send x-aegis-webhook-token. If it cannot set custom webhook headers, place a trusted HTTPS ingress adapter in front that validates provider authentication and adds this dedicated header. Do not expose the webhook route without authentication.

## Environment

Required variables:

| Variable | Purpose |
| --- | --- |
| DATABASE_URL | PostgreSQL URL. Production should use sslmode=verify-full. |
| NATS_URL | tls://host:port; plaintext NATS is rejected. |
| NATS_AUTH_TOKEN | Relay service credential, at least 32 characters. Keep it in a secret manager. |
| NATS_TLS_ROOT_CERT | Optional PEM CA path for a private NATS certificate authority. |
| RELAY_ADMIN_TOKEN | Admin API bearer secret, at least 32 characters. |
| EVOLUTION_BASE_URL | HTTPS Evolution API base URL. HTTP is allowed only for localhost development. |
| EVOLUTION_INSTANCE | Evolution instance name. |
| EVOLUTION_API_KEY | Evolution outbound API credential; held in process configuration only. |
| EVOLUTION_WEBHOOK_SECRET | Dedicated webhook token/HMAC secret, at least 32 characters. |
| RELAY_BIND_ADDR | Defaults to 127.0.0.1:8787. |

In production, bind the process to a private interface and expose the webhook through an HTTPS reverse proxy with request limits and authentication. Do not expose a plaintext or unauthenticated NATS listener. Database initialization creates only the six tables above.

## Local services and smoke test

Local Compose binds PostgreSQL and NATS to loopback only. NATS uses TLS and a development token; its test certificate is generated locally and is not committed. PostgreSQL plaintext is allowed only with RELAY_ALLOW_INSECURE_LOCAL_DATABASE=true and a localhost or Compose-service database host.

From the repository root in PowerShell, run:

    .\relay\dev\start-local.ps1

It starts Postgres and NATS, exports development-only settings, and runs the relay. The local webhook listens only on 127.0.0.1:8787; external webhook traffic still needs HTTPS.

The fake-adapter smoke test needs no provider account, database, or NATS:

    cargo test --manifest-path relay/Cargo.toml --test local_smoke

Or run .\relay\scripts\smoke.ps1. Provider parsing and webhook authentication have fixture-based unit tests.

### Pair an Aegis installation

After the relay is running and Aegis has a locally saved provider/model profile, set the local development credentials in a separate PowerShell session and pair the installation:

    $env:AEGIS_RELAY_ADMIN_TOKEN = "local-development-admin-token-not-for-production"
    $env:AEGIS_NATS_TOKEN = "local-development-nats-token-not-for-production"
    aegis remote pair --relay-admin-url http://127.0.0.1:8787 --admin-token-env AEGIS_RELAY_ADMIN_TOKEN --nats-url tls://localhost:4422 --nats-token-env AEGIS_NATS_TOKEN --nats-root-cert .\relay\dev\certs\nats.crt

Send the one-time `/pair <code>` command printed by Aegis from the WhatsApp number to bind. Then check `aegis remote status` and keep `aegis remote run` running while you use the relay. Pairing stores no credential values; Aegis reads them from the named environment variables. These sample credentials are for local development only. Configure Evolution with a valid account, HTTPS endpoint, API key, and webhook authentication before expecting WhatsApp messages to reach the local relay.

To exercise PostgreSQL pairing rotation, actor-scoped binding lookup, and durable inbound dedupe against the local database, run .\relay\scripts\db-smoke.ps1. It starts only the loopback-bound Postgres service and runs the optional integration test.
