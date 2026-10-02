# Windows PC access and WhatsApp verification

Checked 2026-10-02. Local executable checks and live account checks have separate results.

## Installed build

The release, bundled and globally installed Windows runtime match SHA-256 `eb9e652ffc7f0530e064f2ea59fd3aa01880587dd8f2ab1d70fa046c133c1773`.

Private package packing, file allowlisting, offline installation, postinstall, native equality, command shims, declined onboarding, provider CLI traps, exact PC grants, installed PC status and bundled relay equality passed. Receipt: `.arun/package-smoke-o0t4ZE/receipt.json`. The verified tarball was subsequently installed globally. An open Aegis session locked an earlier installer backup; retaining that package under a unique directory in the same npm root allowed installation without terminating the session. Existing running sessions retain their loaded image. The native `mcp list` command was added after the global check exposed the missing PC status route; its installed regression check passes.

The workspace profile now selects GPT-6.1-sol/high and has the exact ten Windows host grants. Aggregate action/token limit fields were removed; runtime context, response, process and wall bounds remain. Three actual ConPTY tests passed against the installed executable: live output during steering and its slash picker; multiline Unicode editing, command filtering and resize; raw VT clipboard framing.

## Native PC tools

Nine Windows host tests passed, including real stdio MCP discovery, native command failure reporting, bounded Unicode streams, watchdog termination of command descendants, actual PNG dimensions, and exact grants through the actual Aegis installer. Application launch now uses the Windows executable API with literal argument quoting; tests check quotes, empty arguments, trailing slashes, Unicode, newlines and shell-looking strings. A launched PowerShell process completes after the short-lived MCP server exits.

The owned WinForms editor test passed launch, mouse click, foreground verification, focus, Ctrl+A replacement and exact Unicode input. Receipt: `.arun/desktop-input-59388131-f36d-4dda-80d7-06f82adc17dd/receipt.json`. The harness explicitly implements Ctrl+A selection in its multiline control. Earlier diagnostics found a detached Node launch silently exiting before script execution and a normal child dying on server exit. The native launcher fixes both.

Focus accepts an already foreground window and waits for asynchronous activation. Windows can still refuse an activation request; the tool reports that failure. See Microsoft's [foreground window API](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow) and [asynchronous activation explanation](https://devblogs.microsoft.com/oldnewthing/20161118-00/?p=94745).

Actual Aegis task `179aec44-e157-4e23-a68b-4e0228bf4f4b` completed using GPT-6.1-sol/high and only window-discovery/screenshot grants. Its screenshot artifact contains 493,982 bytes, and three requests included the real image. Five recorded responses used 14,415 reported tokens. The ordinary task completed without an explicit requirement ledger. The earlier one-pixel screenshot and internal-root proof loop were fixed rather than treated as successful vision evidence.

## Relay and slash commands

The combined `relay/scripts/e2e-smoke.ps1` gate passed 47 regular tests and both infrastructure targets. These use real PostgreSQL, TLS NATS, the actual relay executable and actual Aegis daemon with deterministic channel/model fixtures. `/help` and an unknown slash command created no model request or run. Group routing, owner alias validation, pending-owner refusal and outbound echo suppression have focused checks. The relay metadata allowlist now contains eight tables, including task conversation reservations and outbound echo hashes.

Authenticated phone commands use the terminal catalog. Interactive pickers and reviews become textual choices; exact approvals and frozen grants remain local authority. Group task scope does not change the self-DM task selection. Ambiguous group creation is retained for reconciliation rather than retried into duplicate groups.

## Current-account setup and remaining live check

Evolution API 2.3.7, Redis, two PostgreSQL services and TLS NATS are healthy in the persistent local stack. The actual Windows relay is running and its authenticated webhook is registered. The linked-device QR has been generated. At the last check the account state was `connecting`, and the task daemon was not running.

Until the linked owner is confirmed, the local relay runs in pending mode and accepts no WhatsApp commands or sends. After the phone scan, `self-account` must confirm an open connection and canonical owner before enabling only the owner's self-chat and known task groups. Send the generated pairing code in **Message yourself**, then start the daemon.

Live phone pairing, real WhatsApp inbound/outbound delivery, and creation of a task group using only the linked account remain unverified. Evolution requires at least one group participant, and the adapter supplies the linked owner. A failed group request keeps the task available in self-chat. No claim of successful carrier delivery or group creation follows from the deterministic relay tests.
