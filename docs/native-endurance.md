# Native Windows endurance validation

The user selected unattended jobs lasting several hours as the first reliability target. This validation used the then-installed Windows Aegis executable and trusted Windows helper, with direct hosted GPT-6 Luna/medium. It did not use Docker or dispatch Aegis through the native Codex CLI. A later October 4 build is now installed; [its README exploration checks](readme-read-loop-2026-10-04.md) are separate from the pinned multi-hour trials below.

`scripts/native-soak.mjs` runs sixteen ordered stages of 450 seconds each: at least two hours of actual elapsed native work. Each stage has a separate 480-second command deadline and emits progress every 30 seconds. The task has a three-hour wall allowance. Aegis must discover the native tool, save checkpoints, keep completed stages in its handoff, execute each stage once, run final verification and actually reach `completed`.

The independent controller retains its own source snapshot, fixture hashes, installed runtime/helper hashes, provider route, event replay and stdout/stderr logs. Its acceptance gate rejects shortened work, missing or overlapping stage receipts, duplicate dispatches, uncertain effects, missing checkpoints, missing final verification, changed fixtures or provider routes, missing live output, insufficient heartbeats and heartbeat gaps over 90 seconds. Short diagnostics receive a different status from the two-hour trial. No status is a general production certificate.

Run explicitly against an installed executable:

```powershell
node scripts/native-soak.mjs --live --binary 'C:\Users\polek\AppData\Roaming\npm\node_modules\aegis-arun\vendor\x86_64-pc-windows-msvc\arun.exe' --model gpt-6-luna --reasoning medium
```

The shorter setup diagnostic adds `--stages 2 --seconds-per-stage 2`. It verifies setup and completion flow, not endurance. The supplied PowerShell fixtures use exclusive receipt creation and flushed writes so repeating a stage cannot overwrite an existing start receipt.

Current trial status: both full native trials are terminal and independently verified. The then-installed `8da8867a...` trial `.arun/native-soak-m7BbH9/` completed sixteen stages in 7,468,040 ms (2h 04m 28s), including 7,201,152 ms of timed work, with 33 model attempts, live output from all stages, no repeated dispatch and no uncertain effect. All three final requirements have current proof at revision 34. Its `receipt.json`, `terminal-audit.json` and installed CLI `final-installed-metrics.txt` retain the evidence. The preceding `76bc4513...` trial `.arun/native-soak-Zu3oMk/` independently completed in 2h 07m 04s. Both preserved runtime hashes were verified. The earlier `.arun/native-soak-vICOgo/` trial failed after two stages and remains failed.

The exact-build manual checkpoint positions were 0, 2, 4, 7, 11, 12, 16 and 16 completed stages. No rolling interval exceeded four. Handoffs at 7 and 11 correctly scheduled stages 8 and 12; there was no manual checkpoint exactly at stage 8. An initial post-auditor required fixed multiples and failed at 8. The independent audit records that discrepancy, the rolling-cadence interpretation and unchanged task/controller/fixtures. This is scoped durability evidence, not a claim of perfect instruction following.

Final installed checks passed four terminal, two native host and two crash-recovery cases on the exact executable. See [the current readiness checklist](native-readiness-checklist.md) for validation scopes and limits.

## Historical observations, 2026-10-03

The entries below retain the statuses observed at their original times. Current terminal results are above.

- Short hosted diagnostic `.arun/native-soak-JYSNHS/receipt.json`: completed two stages with two live-output operations, ten hosted model attempts and no repeated dispatch. It used the earlier receipt-writing fixture; the retained fixture hashes identify that version.
- Thirteen npm tests passed, including acceptance-gate negative cases: `.arun/native-soak-gate-tests-final-20261003.log`.
- Both native recovery checks passed against the installed executable and installed helper: `.arun/native-installed-recovery-20261003.log`. A committed write survives runner death without replay; a write interrupted before its final receipt becomes `outcome_unknown`, does not infer or execute again, and requires reconciliation. The original task, grants, budgets and checkpoint survive. The local HTTP fixtures exercise runtime recovery, not hosted provider recovery.
- Full-duration hosted run `.arun/native-soak-vICOgo/receipt.json` is **running, not accepted**. Installed executable SHA-256 is `8053012b76f535343cea8eb6731dbdd9810b2f78b7382806c16b925999696f4d`; controller snapshot SHA-256 is `55fae6850d73f61dfa10d2e3441363d1bf90ac1f8a4d32fe5739e04a01b1a08f`. Run ID is `1a6df65b-67c5-4f6d-97e6-fc12210d63de`. Start time is 2026-10-03 11:27:57 UTC; two-hour timed work cannot finish before 13:27:57 UTC, and model/checkpoint overhead adds time.

The controller and runner were confirmed alive, and native stage-one heartbeats were observed. Continue observing the existing owned process; a polling timeout is not proof that work stopped. Do not replace the pinned installed runtime or helpers during this trial. The progress receipt does not prove completion: acceptance requires the terminal controller result and independent final checks.

At 11:36 UTC, stage one completed in 450,118 milliseconds and Aegis started stage two without intervention. The controller still reports `running`, one completed stage and zero observation failures. The supervised tool session remains live. The newly added recovery target is also included in an updated full-suite run, `.arun/native-endurance-all-targets-20261003.log`; that suite's result is pending at this entry.

This is a deterministic timed workload that tests uptime, hosted decisions between commands, durable checkpoints and native progress. It does not establish complex coding quality for two hours, arbitrary external-effect deduplication, automatic machine-boot recovery or adversarial isolation. The earlier native Codex/Aegis coding matrix remains separate evidence.

The full-suite continuation completed with **538 tests passed, 11 optional cases ignored across 62 targets**. The hosted trial later failed at 941,116 milliseconds. Both timed stages completed, but the next model connection failed after 20,012 milliseconds, before dispatch; the saved failure has no usage receipt. `.arun/native-soak-vICOgo/events.json` retains the failure and recovery pause. Tool session 98447 exited one. No command or stage was duplicated. This exposes an unattended connection-recovery gap; the runtime currently pauses without a same-route reconnect attempt. The original failed trial will remain a failure even if later recovery or a new trial succeeds.

The repaired native build was installed from `.arun/package-smoke-GGzuko/receipt.json`, executable SHA-256 `76bc45131685205ea682b9ea240e8d46e96ca49bef4eac0ae6f0f120c1f2372d`. It adds bounded pre-dispatch same-route retries while preserving the original task deadline. The final source suite passed 549 tests, with 11 optional ignored cases across 64 targets; installed terminal, PowerShell and crash-recovery checks also passed.

A separate fresh full trial is now running in `.arun/native-soak-Zu3oMk/`, run `4f92bc9b-b11a-4e66-8405-e487871c4705`, under owned tool session 97974. Its controller PID is 13556 and runner PID 37000. The controller pins the new installed executable/helper and its source snapshot. The earlier two-stage failure stays a failure; this trial must still finish sixteen stages, verify the receipts and reach actual completion before endurance can pass. The initial sandbox process-creation denial was resolved through an approved native execution rerun.
