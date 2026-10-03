# Aegis terminal UI

Research checked: 2026-10-03. This document records implemented behavior and verification, separately from future ideas.

The October 4 build is globally installed with executable SHA-256 `b93d30f9e0dc1846def6fc085e64cd36ab37a40125dda36f026b988fe69f2bf0`. Final installed checks passed four real ConPTY cases, two native host cases and two crash-recovery cases: `.arun/read-loop-global-pty-20261004.log`, `.arun/read-loop-global-host-20261004.log` and `.arun/read-loop-global-recovery-20261004.log`. The complete source suite passed 554 tests with 11 explicit ignores across 64 targets, and all 13 npm checks passed. See [the README exploration repair](readme-read-loop-2026-10-04.md) and `.arun/read-loop-installed-20261004.json`. Older hashes below describe historical trials.

## Persistent input and live activity

Following a running task opens the composer immediately. Live output continues during typing and the slash picker. The input border retains activity, elapsed time and the current UTC clock, with phase-specific animation and rotating keyboard tips. `AEGIS_REDUCED_MOTION=1` keeps animation static. Enter submits steering to the existing task; Ctrl+D detaches, Ctrl+C targets interruption, and F9 opens cancellation confirmation.

Closing the composer reports the actual saved state. Interrupted work says `Task needs recovery`, paused work says `Task paused`, and only completed work says `Task completed`; a conversational response says `Reply finished`.

Commands display their literal arguments; trusted PowerShell previews display the script and readable stdout/stderr. Large output retains its artifact recovery path. Precautionary workspace revision changes say whether any verified requirements actually became stale.

Native PowerShell stdout/stderr now appears during execution through bounded, request-correlated MCP progress notifications. Preview is limited to 64 KiB or 128 messages; final tool receipts remain execution evidence. A model-response pause identifies an exhausted response deadline or task wall deadline when applicable.

Public provider reasoning summaries and commentary appear as they arrive, before final action processing. Hidden internal reasoning is unavailable. Progress narration is not verification evidence. ChatGPT requests summary output when reasoning is enabled; providers that do not supply summaries can still display commands and execution results.

Token usage remains in the live composer border while typing and using the slash picker. `t` counts provider-reported input plus output, `~t` marks custom-endpoint estimates, and `t?` marks legacy usage whose source is unclassified. Cached input belongs to input and is not added again. An outstanding request shows `usage pending`; counts update only from durable receipts. Current requirement verification appears alongside usage and stops counting proof during an in-flight workspace mutation or after the workspace revision changes.

`/metrics` opens the detailed report without inference. The installed build also exposes `aegis metrics <run-id>` from the shell. The same report appears when a task ends in the interactive follower: input/output/cached usage, estimates, missing receipts, current requirement evidence, independent acceptance, tool outcomes, elapsed time, output tokens per second of recorded request time, and tokens per completed task. Missing usage or timing prevents an exact efficiency ratio. Historical acceptance with stale evidence is labelled accordingly. These figures describe observed work; requirement coverage is not a model confidence score. Live elapsed time and usage stay visible while the detailed report remains available on demand.

Connection retries appear in the timeline and live phase. Only typed connection failures before dispatch receive up to three same-route retries, with 1/2/4-second backoff and the original deadline. A pause or cancellation interrupts backoff. Dispatched requests without usage are retained as unknown and do not qualify for this retry.

Action-shaped JSON and code-fenced commentary are omitted from the progress feed. Only the provider's validated final action can create an operation; command history and results come from committed operation events. Commentary cannot execute a command.

Aegis asks when intent or a decision is unclear, while continuing independent work. Pending questions remain visible beside input and survive restart. Enter answers the oldest pending question and steers the same task. A question wait can resume after its answer; unrelated recovery pauses still require reconciliation. Unanswered questions prevent completion.

The privately installed Windows executable with SHA-256 `b0d8cd3bf447276d82dae2898199bace042b9f4db37621239aa24e36dd1676c2` passed three real ConPTY tests and one screen-decoder regression twice: `.arun/question-archive-private-pty-corrected-20261002.log` and `.arun/question-archive-private-pty-stability-20261002.log`. These cover immediate input visibility, activity while typing, streamed public-summary rendering, a pinned question and submission on the original task. The UI cases use local fixtures; the matching hosted GPT-6 Luna/medium streaming receipt is `.arun/hosted-output-live-EQ9vu5/receipt.json`. On 2026-10-03 the previously running older process was absent; global installation succeeded with the same binary hash, and all four terminal checks passed against the global executable: `.arun/question-archive-global-pty-20261003.log`.

## Reference patterns

| Reference | Pattern to adopt | Aegis decision |
| --- | --- | --- |
| [Codex interactive CLI](https://learn.chatgpt.com/docs/codex/cli) | Visible commands and diffs during work; steer the current task | Stream durable operation output and keep polling while composing steering text |
| [Codex developer commands](https://learn.chatgpt.com/docs/developer-commands?surface=cli) | Searchable slash command popup and input history | Derive the picker from the complete command catalog; filter it locally; preserve drafts and caret positions during history navigation |
| [Claude Code interactive mode](https://code.claude.com/docs/en/interactive-mode) | Multiline editing, command filtering, readable code, detailed task feedback | Use grapheme-aware wrapped editing, normalized bracketed paste, code indentation and diff colors, active clocks and completion timestamps |

These are product design choices, not a claim of feature parity. The reference clients have different queue and interrupt shortcuts. Aegis exposes its own controls in the composer and task follower rather than claiming their keymaps are identical.

## Runtime behavior

- `/` opens the command catalog. Typing filters; arrows and scrolling reveal remaining commands. Selecting a command fills the prompt for review.
- The composer wraps to the current terminal width. Shift+Enter or Ctrl+J inserts a line; pasted line breaks remain visible. Cursor movement and deletion respect grapheme boundaries.
- Steering messages can be written while work continues. Live events continue through the steering composer and its slash picker. Ctrl+C in the active picker requests task interruption; Escape closes the picker.
- Process output appears before the process ends. A bounded preview protects the terminal; reaching its limit displays an artifact recovery notice. Full captured output remains evidence.
- Terminal dimensions are measured repeatedly, in addition to resize events. Composer and picker reflow and clear their prior rows.
- Active feedback shows elapsed time and current UTC time. Terminal completion records its timestamp and elapsed time.
- Aegis owns execution, obligations, permissions, evidence and recovery. Its subscription transport does not delegate tasks to the Codex CLI or Claude Code.
- Token and action usage are measured without aggregate token or turn limits. Context, response bytes, process duration and wall time remain separate resource controls.

## Verification

The live input correctness commit `7651a13` passed 41 focused terminal tests. Earlier tests exercised layout helpers but missed the actual composer newline mapping; the regression now covers that mapping. Typed picker outcomes distinguish cancellation, interruption and task completion.

The Windows input repair passed 58 focused library tests. Three end-to-end tests also passed against a rebuilt executable through a real Windows pseudo-console (ConPTY):

- Native Windows Terminal keyboard protocol: multiline Unicode paste stays in the composer until Enter; Shift+Enter and Ctrl+J insert visible lines; wrapping and resize reflow; the complete 39-command catalog scrolls and filters.
- Raw VT input: multiline Unicode clipboard input stays in the composer without accidental task submission.
- Active follower: output continues during steering and its nested command picker; a capped preview displays its artifact recovery notice; Ctrl+C durably requests operation interruption; completion feedback appears.

These terminal tests use deterministic local fixtures. Hosted checks below exercise real model calls separately.

Hosted checks completed using an actual rebuilt Aegis executable with SHA-256 `7bfc0b4c7c7d798f20f45bfb8249ab4f0b467f543bc6ebe9f0023ef089fafd2e`:

- Parser task `148de391-4439-4c22-a75c-acba7eb61d11`: paused during real inference, resumed the same durable run, preserved its route and frozen contract, added nested expression support and regression tests, passed independent acceptance, and verified all four explicit requirements at revision 6. Fifteen recorded responses; 59,448 provider-reported tokens. Receipt: `.arun/hosted-control-live-WsYOCU/receipt.json`.
- Output task: first output was durable before command success; the live preview reached its cap; the referenced raw artifact retained all 70,033 bytes, including the final marker beyond that cap. Requested model and high effort matched every recorded route. Receipt: `.arun/hosted-output-live-QL6BRc/receipt.json`. The verifier initially inspected the process receipt instead of its referenced output artifact; the corrected verifier audited the retained trial without another model run.

The Windows tests initially exposed native input splitting clipboard lines into Enter events and losing the first key during a temporary cursor-position read. The repaired input adapter owns one reader for the guided session, frames clipboard input atomically, and supports both negotiated native keyboard packets and raw VT input. The passing tests above cover the corrected build.

A later hosted output check passed with executable SHA-256 `81b19cbdca8025c6300a53ba047c45e5c000f03f118d4b697062c0ec455b0f8f`: both the durable output event and its foreground CLI display appeared before process success. The raw artifact retained all 70,033 bytes, and both model calls used GPT-6.1-sol/high. Receipt: `.arun/hosted-output-live-3iNe68/receipt.json`. This verifies the foreground runner and display execute concurrently, rather than replaying output after work ends.

The subsequent library regression gate passed 377 tests with three hosted-only checks explicitly ignored. The control-surface, file-freshness and obligation-contract integration targets passed 12 tests. This includes rejection of process proof after an edit during execution, idempotent terminal invalidation, fresh proof after external changes, reviewed legacy adoption and unbounded aggregate usage accounting. Output sanitation now retains a bounded parser state for each operation so ANSI and OSC sequences split across live chunks cannot leak control fragments or hidden payload into the transcript.

## Installed Windows package

Final audit on 2026-10-02: the current optimized runtime, bundled executable and global installation match SHA-256 `eb9e652ffc7f0530e064f2ea59fd3aa01880587dd8f2ab1d70fa046c133c1773`. The current library gate passed 393 tests, with three hosted-only cases explicitly ignored. The setup, control-surface, token accounting and installed ConPTY targets passed 12 tests. The terminal follower check now submits an actual message with Enter and verifies it is queued on the original task, then exercises output during its slash picker, interruption, and completion during a new draft.

The installed executable also completed a fresh GPT-6.1-sol/high streaming task: both durable output and foreground display appeared before process success; exactly one marker command succeeded; the full artifact retained all 70,033 bytes. Receipt: `.arun/hosted-output-live-OxFK71/receipt.json`. Private package receipt: `.arun/package-smoke-o0t4ZE/receipt.json`. The workspace profile selects GPT-6.1-sol/high and contains no aggregate token/action cap fields. The records below describe earlier verified builds.

The optimized Windows runtime was rebuilt and installed globally from the verified local npm tarball. `aegis.cmd --version` reports `aegis 0.1.0`. Its installed executable matches the release and packaged SHA-256 `3186b1ee3b6e2b2f6eeb3aae493d7914c1121fb3651ff7c7de734d75e9d13c1c`.

- The three ConPTY tests passed again with `AEGIS_PTY_BINARY` pointing to the installed executable, including actual distinct foreground color roles, a current UTC clock, elapsed time and the persisted completion timestamp.
- The release executable passed hosted GPT-6.1-sol/high streaming verification with both output events and foreground display observed before process success. Full output remained intact. Receipt: `.arun/hosted-output-live-oAffet/receipt.json`.
- Private npm packing, file allowlisting, postinstall, command shim, binary equality and declined onboarding/sign-in passed. The fixture traps confirmed no native provider CLI ran. Receipt: `.arun/package-smoke-4mUNpH/receipt.json`. Global installation then exited successfully, and its executable hash was checked separately.
- Twelve npm unit tests passed. These checks cover this Windows build and local installation; they do not certify a new public npm release or Linux builds.
- The installed `aegis.cmd models chatgpt --refresh` returned GPT-6.1-sol first, with high reasoning among its advertised levels. This checks the account catalog rather than assuming availability from a static list.

Open a new `aegis` session to load the rebuilt executable. An already running process retains its loaded image.

## Requirement audit

| Requested behavior | Evidence |
| --- | --- |
| Visible tool/action output during work | Hosted release check observes both the event and printed marker before command success; full output artifact is checked separately |
| Overflow wraps and pasted text stays intact | Installed ConPTY tests exercise Unicode paste, explicit lines, long-word wrapping, native and raw VT input, and no submission before Enter |
| Steer and interrupt while work continues | Installed follower test submits steering to the existing task, renders output during typing and its slash picker, and checks the targeted durable interruption request; kernel tests verify pending messages enter model context; hosted parser check pauses and resumes real inference |
| Fullscreen/resize responds | Actual pseudo-console width and height changes reflow the composer and picker; library cases cover height-only changes and clearing old rows |
| More colors | Installed child runs with colors enabled and emits at least three distinct foreground palette roles; diff/code formatting tests check intended row styles |
| Worked time, current time and completion time | Installed follower screen contains current UTC time and elapsed work time; completion displays the timestamp recorded in the durable event and elapsed time |
| `/` reveals all choices and filters | Complete command-catalog integration check plus installed picker navigation through all 40 entries, including `/metrics`, filtering and resize |
| No aggregate token or turn budgets | Library, usage-view, setup and token-budget integration checks; the real parser run records 15 responses and 59,448 tokens without task caps |
| Own Aegis runtime and truthful identity | Direct transport request tests, persisted route identity tests and installed package traps reject native provider CLI execution |
| Small atomic commits and subagents | Renderer, Windows input, foreground streaming, evidence repair, sanitizer and acceptance additions are separate commits; independent agents reviewed and exercised the input, UI and obligation paths |

## Further ideas

Detailed transcript search, a visible steering queue, theme selection and syntax-aware code coloring need their own design and acceptance checks. Current code block formatting and diff colors should not be described as a complete syntax highlighter or transcript browser.
