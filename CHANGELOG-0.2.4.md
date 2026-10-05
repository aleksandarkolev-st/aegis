# Aegis 0.2.4

`0.2.3` → `0.2.4`. Runtime reliability for task recovery and long explorations.

## Runtime fixes

- Answering a blocking question gives a newly spawned runner its own bounded
  startup window. The follow session no longer mistakes an asynchronous lock
  acquisition for a stopped runner.
- Exploration progress follows returned search results, inspected excerpts and
  unread file ranges. Changing query text or revisiting known evidence does not
  keep a task alive indefinitely.
- A durable exploration allowance pauses open-ended work after 24 actions
  without an accepted, evidence-backed finding. New owner input, changed
  workspace content or a new supported finding renews the allowance.

## Verification

The Rust all-targets suite passed with 617 tests passed and 11 ignored. The
release workflow builds Windows x64, Linux x64 and Linux arm64 binaries, then
creates the GitHub release before publishing the matching npm package.
