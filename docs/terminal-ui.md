# Aegis terminal UI

Research checked: 2026-10-02. This document records implemented behavior and verification, separately from future ideas.

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

## Further ideas

Detailed transcript search, a visible steering queue, theme selection and syntax-aware code coloring need their own design and acceptance checks. Current code block formatting and diff colors should not be described as a complete syntax highlighter or transcript browser.
