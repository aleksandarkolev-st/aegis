# Local WhatsApp setup on Windows

This persistent stack uses Evolution API **2.3.7**, Redis, separate PostgreSQL databases for Evolution and relay metadata, and TLS NATS. Evolution runs in Docker Desktop; the real relay and installed Aegis run on Windows. The development smoke fixtures are separate.

From the Aegis workspace, run:

```powershell
aegis whatsapp init
aegis whatsapp start
aegis whatsapp create-instance
aegis whatsapp qr
```

Open the printed local `whatsapp-qr.html` path. On the phone hosting the relay account, open **WhatsApp → Settings → Linked devices → Link a device** and scan. No account is linked by the setup commands alone.

For your existing single WhatsApp account, enable `self-account` after linking. It reads the linked owner from Evolution and enables only your own self-DM and own messages in known task groups. It refuses to activate before the account reports a connected owner. Then run `pair` to generate the five-minute Aegis pairing code and send it in **Message yourself**:

```powershell
aegis whatsapp self-account
aegis whatsapp pair
aegis whatsapp daemon
aegis whatsapp status
```

After sending the code to yourself, run `daemon`. Replies are protected against being mistaken for new self-account commands. Task groups accept only their paired owner's messages and route to the exact task; unrelated groups are ignored. The default adapter mode remains available for deployments with a separate relay account.

Send `/help` in WhatsApp for the terminal command catalog, `/goal <task>` to start work, and `/new` in self-chat before a separate task. Interactive terminal pickers become text choices and explicit reviews in WhatsApp. Group creation with only the linked owner is still unverified; a failed request leaves that task controllable in self-chat.

To grant files, native PowerShell and desktop applications to new workspace tasks, run `aegis pc enable --trusted-host`. It registers the included Windows MCP server and exact tool grants. Choose your model and reasoning in local settings, or use `/model gpt-6.1-sol` and `/reasoning high` for future remote tasks. Remote host operations still use Aegis's exact approval gates.

`webhook` re-registers the authenticated inbound route. `stop` stops owned Windows processes and Docker services while retaining sessions and volumes. `start` resumes the infrastructure and relay; run `daemon` again if paired. A Windows restart requires these start commands. Aegis still applies its local permissions and approval gates to remote requests.

Use `-Workspace <directory>` for another workspace, `-AegisBinary <installed arun.exe>` and `-RelayBinary <aegis-relay.exe>` if native executable discovery is unavailable. Bundled relay executables are used before the repository Cargo fallback. Initial setup reads the installed executable's real workspace identity. Re-running `init` preserves credentials; deleting state or Docker volumes is not an upgrade procedure.

## Local data and network

Random credentials, certificate/private key, process identity records, private logs and QR artifacts live in the Git-ignored `.arun/local-remote` directory. Its Windows ACL grants only the current user and SYSTEM, including inherited files. Compose container environment values remain visible to Docker administrators.

Published ports bind to `127.0.0.1`. Redis and Evolution's PostgreSQL have no host ports. Evolution uses `host.docker.internal` to reach the loopback Windows relay; webhook setup verifies this route before registration. NATS permits only the relay principal and the exact local device's command consumer, event subject and private inbox. The local relay PostgreSQL connection uses the explicit loopback plaintext exception; NATS always uses TLS.

Evolution keeps its authentication/session data in persistent Docker volumes. Message/contact/chat/history database persistence and verbose webhook logs are disabled. This does not establish a live delivery guarantee: phone linking and an actual incoming/outgoing WhatsApp exchange must be verified after scanning.

## Version and API references

The image tag follows Evolution's [2.3.7 official deployment configuration](https://github.com/evolution-foundation/evolution-api/blob/2.3.7/Docker/swarm/evolution_api_v2.yaml). Storage/log switches follow its [versioned environment example](https://github.com/evolution-foundation/evolution-api/blob/2.3.7/.env.example). Instance creation and QR retrieval use the [versioned instance routes](https://github.com/evolution-foundation/evolution-api/blob/2.3.7/src/api/routes/instance.router.ts). The webhook uses `MESSAGES_UPSERT`, `byEvents=false`, and a custom `x-aegis-webhook-token` header, supported by the [versioned webhook controller](https://github.com/evolution-foundation/evolution-api/blob/2.3.7/src/api/integrations/event/webhook/webhook.controller.ts).

Task group creation uses `POST /group/create/{instance}` with the deterministic task subject and your phone as the participant. Evolution's [2.3.7 group schema](https://github.com/evolution-foundation/evolution-api/blob/2.3.7/src/validate/group.schema.ts) requires at least one participant; an empty array is invalid. The [Baileys adapter](https://github.com/evolution-foundation/evolution-api/blob/2.3.7/src/api/integrations/channel/whatsapp/whatsapp.baileys.service.ts) passes the resolved participants to WhatsApp group creation. Creating a group with the linked owner must still be verified against the actual linked account; a failed or ambiguous creation is reported in the task's DM rather than silently claiming a group exists.
