# `plan-text.txt` vertical-slice E2E audit

Status checked 2026-10-02 against the relay sources and the successful combined E2E run. This file records what the tests exercise; it does not change the plan.

## Scope

The first vertical slice in `plan-text.txt` has eight requirements:

1. Aegis remote daemon connects outbound to JetStream.
2. A small relay handles channel webhooks, identity/routing, pairing, and notifications.
3. WhatsApp is the only channel in this first slice.
4. A user pairs a phone to one Aegis installation.
5. The phone can send a message and issue `status`, `pause`, `resume`, and `cancel`.
6. Aegis can ask for approval, and the phone can approve once or deny that exact operation.
7. The phone receives completed and failed task notifications.
8. Remote permissions stay narrower than local permissions.

The associated architecture gates are that the local kernel and SQLite remain authoritative; the daemon uses outbound TLS; PostgreSQL contains relay metadata rather than workspace/model state; duplicate deliveries are safe; actor and installation boundaries hold; and notifications expose only bounded, safe task information.

The user explicitly rejected macOS. Native iMessage, a Mac bridge, and macOS-specific tests are excluded from scope, even though the plan text describes iMessage as a possible later adapter. Attachments, remote terminal access, diffs, provider configuration, Matrix, multiple hosts, voice, group chats, and elaborate buttons are also outside this first slice as the plan says.

## Current E2E evidence

`relay/scripts/e2e-smoke.ps1` builds and invokes the actual `arun` binary, starts isolated PostgreSQL and TLS JetStream services, runs the relay suite, and invokes the ignored test `relay/tests/local_smoke.rs::jetstream_remote_phone_lifecycle_uses_local_authority_and_isolates_devices`. The combined run exited 0 with 37 regular tests and 2 infrastructure targets passing: the expanded Aegis-daemon lifecycle and `relay/tests/binary_startup.rs::actual_relay_binary_uses_evolution_http_and_real_postgres_tls_nats`.

The Aegis lifecycle test runs the pairing CLI and remote daemon, a real PostgreSQL repository, a TLS JetStream transport with installation-specific credentials, and the relay HTTP router. It pairs a one-time code, sends a message that starts a task, waits for a write operation to be gated, stops the daemon, sends approval for the exact challenge, restarts the daemon, and verifies the queued approval is applied, one write succeeds, the task completes, a completion notification is delivered, and `/result` returns a bounded result. The expanded case also routes status, task selection, steering, pause/resume/cancel, and denial through the webhook, TLS JetStream, and actual daemon; checks private-task denial; verifies denial cannot be reversed; and confirms a phone approval cannot expand read-only frozen grants or budgets. It also checks a second NATS device cannot inspect or publish on the first device's subjects.

The expanded lifecycle also verifies delivery of a `run.failed` notification after the daemon initializes its event cursor. That failure event is injected synthetically, so this proves event-to-channel delivery but not failure generation from a real task execution.

The separate relay-process test starts the actual `aegis-relay` executable, exercises health and admin pairing, authenticates and normalizes an Evolution-format inbound webhook using a local HTTP mock, checks command publication over TLS NATS, and returns a device event through the actual local HTTP `sendText` request. It checks delivery deduplication and provider receipt metadata. It also asserts the exact six public metadata tables and their columns, then confirms serialized database rows contain none of the inbound command sentinel, outbound reply sentinel, or plaintext pairing token.

The combined tests use deterministic HTTP/channel/model fixtures for reproducibility. They cover the actual local Evolution HTTP request/response boundary, not a live Evolution account, WhatsApp carrier delivery, or an external relay deployment. Do not describe them as live WhatsApp delivery.

## Requirement coverage

| Plan requirement | Current evidence | Status / remaining E2E gap |
| --- | --- | --- |
| Outbound JetStream daemon | Isolated lifecycle uses the real daemon, TLS NATS, a root certificate, and device-scoped NATS credentials. It checks that a second device cannot read/create the first device's consumer or publish its events. | Covered for the local TLS test deployment and per-installation NATS ACLs. Production Internet connectivity and deployed relay configuration are not exercised. |
| Small relay with pairing and routing | `binary_startup.rs::actual_relay_binary_uses_evolution_http_and_real_postgres_tls_nats` starts the actual relay executable, tests the authenticated Evolution HTTP boundary, pairing, command publishing, reply delivery, and receipts. | Covered at the local executable and gateway-contract boundary. External deployment is not tested. |
| WhatsApp channel | The relay-process test uses a mock Evolution HTTP endpoint and checks the exact local sendText URL and body; the Aegis lifecycle uses a deterministic WhatsApp channel fixture. | Partial: live Evolution credentials/account and WhatsApp carrier delivery are not tested. |
| Pairing | Real Aegis `remote pair` CLI and the actual relay-process test redeem a one-time code against PostgreSQL. Pairing rotation and expiration are also exercised in repository tests. | Covered for the local path. Live WhatsApp delivery of the pairing message remains external and unverified. |
| `message`, `status`, `pause`, `resume`, `cancel` | The expanded lifecycle sends these controls through webhook, TLS JetStream, and the actual Aegis daemon; task selection and steering are also exercised. | Covered for the deterministic local test setup. The model endpoint is a fixture, not a live model provider. |
| Approval, approve once, deny | The lifecycle approves the exact write challenge across a daemon outage/restart, denies an exact seeded write, checks denial stays final, and confirms approval cannot expand a read-only frozen grant. | Covered for the tested operation and grant cases. |
| Completed and failed notifications | The lifecycle delivers completion and a synthetic `run.failed` notification through the actual daemon/JetStream/relay path. | Covered for notification delivery. The failed event is injected; execution-time failure generation is not verified here. |
| Narrow remote permission policy | Lifecycle checks private-task denial, exact-operation approval/denial, immutable denial, read-only frozen grants, and unchanged permission/budget state. The local authority fixture also checks actor-scoped routing and redaction. | Covered for these tested boundaries. Arbitrary command and credential-retrieval attempts are not separately sent through this lifecycle. |

## Cross-cutting guarantees

- **Local authority:** The isolated lifecycle verifies that the actual daemon leaves the write pending until the exact approval arrives and that the workspace write and completion are recorded in local Aegis state.
- **Durability and idempotency:** The lifecycle stops and restarts the daemon while an approval is queued, then observes a single successful write. Separate tests cover duplicate webhook receipts, duplicate local command application, provider send retries, and PostgreSQL receipt behavior. A repeated command through the complete live lifecycle is not separately asserted.
- **PostgreSQL minimization:** The relay-process E2E asserts the exact six public metadata tables/columns and that exercised rows omit command text, reply text, and plaintext pairing tokens. This is an allowlist and sentinel check over the tested flow, not an assurance about all future schema or data paths.
- **Actor and installation isolation:** The daemon lifecycle checks private-task denial and installation-specific NATS subjects. A local authority fixture checks actor-scoped notification routing. The lifecycle does not cover every possible multi-phone policy combination.
- **Notification privacy:** Completion and synthetic failure use bounded event rendering; the local authority fixture checks model-authored secret text is not exposed. The relay-process database checks that reply text is not persisted in metadata.

Relevant existing tests include:

- `relay/tests/local_smoke.rs::jetstream_remote_phone_lifecycle_uses_local_authority_and_isolates_devices`
- `relay/tests/binary_startup.rs::actual_relay_binary_rejects_missing_configuration`
- `relay/tests/binary_startup.rs::actual_relay_binary_uses_evolution_http_and_real_postgres_tls_nats`
- `relay/tests/local_smoke.rs::relay_messages_use_aegis_local_authority_and_safe_events_return_to_the_paired_actor`
- `relay/tests/local_smoke.rs::local_whatsapp_to_aegis_and_reply_delivery_smoke`
- `relay/tests/local_smoke.rs::used_pair_code_retry_is_deduplicated_and_never_published_as_a_command`
- `relay/tests/local_smoke.rs::ambiguous_provider_send_retries_at_least_once_then_receipt_deduplicates`
- `relay/tests/postgres_receipts.rs::postgres_pairing_rotation_and_inbound_receipts_are_idempotent`
- `relay/tests/admin_revoke.rs::admin_revoke_is_scoped_and_blocks_commands_from_the_revoked_phone`
- `src/remote/mod.rs::supported_commands_are_local_and_do_not_expand_run_permissions`

## External acceptance boundary

The local isolated suite can verify Aegis, relay code, PostgreSQL, and TLS JetStream without outside accounts. Evolution/WhatsApp gateway and carrier delivery should be called verified only after an authorized live account test exercises pairing, inbound command delivery, and an outbound state notification. No such live external test was available during this audit. iMessage/macOS is excluded by the user's instruction and is not a release gate.
