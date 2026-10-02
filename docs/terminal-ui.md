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

Real Windows pseudo-console tests and hosted GPT-6.1-sol/high verification are required before declaring the interactive work complete. Their results will be recorded here after execution.

## Further ideas

Detailed transcript search, a visible steering queue, theme selection and syntax-aware code coloring need their own design and acceptance checks. Current code block formatting and diff colors should not be described as a complete syntax highlighter or transcript browser.
