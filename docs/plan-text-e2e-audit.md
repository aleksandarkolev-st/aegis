# `plan-text.txt` vertical-slice E2E audit

Status checked 2026-10-02 against the relay sources and test suite after the isolated relay E2E passed. This file records what the tests exercise; it does not change the plan.

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

`relay/scripts/e2e-smoke.ps1` builds and invokes the actual `arun` binary, starts isolated PostgreSQL and TLS JetStream services, runs the relay suite, and invokes the ignored test `relay/tests/local_smoke.rs::jetstream_remote_phone_lifecycle_uses_local_authority_and_isolates_devices`. Root reported the script passed 36 non-ignored tests plus that isolated lifecycle test.

The isolated lifecycle test runs the Aegis pairing CLI and remote daemon, a real PostgreSQL repository, a TLS JetStream transport with installation-specific credentials, and the relay HTTP router. It pairs a one-time code, sends a message that starts a task, waits for a write operation to be gated, stops the daemon, sends approval for the exact challenge, restarts the daemon, and verifies the queued approval is applied, one write succeeds, the task completes, a completion notification is delivered, and `/result` returns a bounded result. It also checks a second NATS device cannot inspect or publish on the first device's subjects.

The Windows verification agent reports that a follow-up to this same lifecycle is in progress for shared/private task selection, status, steering, pause/resume/cancel, exact-operation denial, approval under read-only frozen grants, and failed notifications. These cases are not counted as passing until the change lands and the E2E run passes. The failed-notification case uses a synthetic local failure after event-cursor initialization, so it verifies event-to-channel delivery rather than proving that a real agent task can fail and report it.

This is a real Aegis/relay-core/JetStream/PostgreSQL integration path, but it uses a `FakeProvider` with a test header and a local fixture model endpoint. The test hosts the relay router in-process; it does not launch the separately configured `aegis-relay` server process. It does not verify a live Evolution account, WhatsApp carrier delivery, or an external relay deployment. Do not describe it as live WhatsApp delivery.

## Requirement coverage

| Plan requirement | Current evidence | Status / remaining E2E gap |
| --- | --- | --- |
| Outbound JetStream daemon | Isolated lifecycle uses the real daemon, TLS NATS, a root certificate, and device-scoped NATS credentials. It checks that a second device cannot read/create the first device's consumer or publish its events. | Covered for the local TLS test deployment and per-installation NATS ACLs. Production Internet connectivity and deployed relay configuration are not exercised. |
| Small relay with pairing and routing | The lifecycle exercises the relay HTTP router, `RelayService`, real PostgreSQL, pairing, command publishing, and event delivery. | Partial: the E2E does not start `aegis-relay` as a separate process, so its executable startup/configuration boundary is not covered end to end. |
| WhatsApp channel | Lifecycle uses a fake channel implementation reporting `ChannelId::WhatsApp`; the simpler fake webhook tests exercise parser/auth/routing behavior. | Partial: provider/carrier integration is not tested. Evolution credentials and a live authorized WhatsApp test account are required to verify that external gateway. |
| Pairing | Real Aegis `remote pair` CLI creates a code; a fake WhatsApp webhook redeems it against PostgreSQL and acknowledges pairing. PostgreSQL tests also cover pairing rotation. | Covered for the local relay path; live WhatsApp delivery remains external and unverified. |
| `message`, `status`, `pause`, `resume`, `cancel` | Full lifecycle sends a message and later `/result`. Parser and local-authority tests cover command parsing/effects for the other controls. | Partial: `/status`, `/pause`, `/resume`, and `/cancel` do not yet traverse phone webhook to PostgreSQL/JetStream to running daemon to phone reply in the isolated lifecycle. Root's Windows E2E work is extending this control coverage. |
| Approval, approve once, deny | Full lifecycle sends an approval request and approves the exact challenge while the daemon is down, then verifies delivery after restart. A separate direct-authority fixture covers denial and expiry. | Partial: `/deny` is not yet exercised end to end through the isolated JetStream daemon lifecycle. Root's Windows E2E work is adding lifecycle decision coverage. |
| Completed and failed notifications | Full lifecycle observes a completion notification. Unit code maps `run.failed` to a failed event; relay event-model tests check safe formatting. | Partial: no failed task currently traverses the daemon to JetStream to relay to channel delivery path. Root's Windows E2E work is extending notification coverage. |
| Narrow remote permission policy | Full lifecycle proves a write remains pending until its exact approval and that the NATS installation boundary holds. `relay_messages_use_aegis_local_authority_and_safe_events_return_to_the_paired_actor` checks actor-scoped task access and safe output with a direct authority fixture. `supported_commands_are_local_and_do_not_expand_run_permissions` checks local command effects. | Partial: the isolated lifecycle does not yet send denied cross-actor/task, arbitrary command, permission-expansion, or secret-retrieval attempts through the full phone-to-daemon route. The existing fixture/unit coverage is valuable but is not the same end-to-end boundary test. |

## Cross-cutting guarantees

- **Local authority:** The isolated lifecycle verifies that the actual daemon leaves the write pending until the exact approval arrives and that the workspace write and completion are recorded in local Aegis state.
- **Durability and idempotency:** The lifecycle stops and restarts the daemon while an approval is queued, then observes a single successful write. Separate tests cover duplicate webhook receipts, duplicate local command application, provider send retries, and PostgreSQL receipt behavior. A repeated command through the complete live lifecycle is not separately asserted.
- **PostgreSQL minimization:** The E2E uses PostgreSQL for relay metadata and pairing. Schema and repository tests cover metadata operations, but the lifecycle does not assert a database-wide allowlist or scan for accidental task/workspace/model data.
- **Actor and installation isolation:** The isolated JetStream check proves installation-specific NATS subjects. A separate local authority fixture checks task and notification routing between actors. A multiple-phone, same-installation command scenario is not part of the isolated lifecycle yet.
- **Notification privacy:** The completed message is bounded and the local authority fixture checks that model-authored secret text does not enter the notification. A full failed-notification privacy check is still needed with the failure delivery case.

Relevant existing tests include:

- `relay/tests/local_smoke.rs::jetstream_remote_phone_lifecycle_uses_local_authority_and_isolates_devices`
- `relay/tests/local_smoke.rs::relay_messages_use_aegis_local_authority_and_safe_events_return_to_the_paired_actor`
- `relay/tests/local_smoke.rs::local_whatsapp_to_aegis_and_reply_delivery_smoke`
- `relay/tests/local_smoke.rs::used_pair_code_retry_is_deduplicated_and_never_published_as_a_command`
- `relay/tests/local_smoke.rs::ambiguous_provider_send_retries_at_least_once_then_receipt_deduplicates`
- `relay/tests/postgres_receipts.rs::postgres_pairing_rotation_and_inbound_receipts_are_idempotent`
- `relay/tests/admin_revoke.rs::admin_revoke_is_scoped_and_blocks_commands_from_the_revoked_phone`
- `src/remote/mod.rs::supported_commands_are_local_and_do_not_expand_run_permissions`

## External acceptance boundary

The local isolated suite can verify Aegis, relay code, PostgreSQL, and TLS JetStream without outside accounts. Evolution/WhatsApp gateway and carrier delivery should be called verified only after an authorized live account test exercises pairing, inbound command delivery, and an outbound state notification. No such live external test was available during this audit. iMessage/macOS is excluded by the user's instruction and is not a release gate.
