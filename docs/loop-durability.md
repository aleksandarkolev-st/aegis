# Long-running task memory

Task-owner messages have their own transactional projection. Archiving audit events does not remove pending messages or delivered constraints. Each inference captures a delivery cursor before building its prompt; its response cannot acknowledge later arrivals. Existing stores migrate once from integrity-checked hot and archived events.

Small messages remain verbatim. Larger accumulated input is presented as an ordered bounded batch, with the newest corrections also visible. Clipped messages expose exact same-run `user:<sequence>` handles. `inspect_result(handle, "@full")` retrieves a complete owner message; a committed full retrieval is required before compacting an oversized message. The agent can search or page the lossless message index using `inspect_result("user", query)` or `"@after <sequence>"`.

`remember(summary, artifact="user:<owner_batch_through>")` saves up to 8,192 UTF-8 bytes of working memory. It merges prior constraints, findings, decisions, failed approaches and next work, covering only the displayed contiguous source batch. Large messages are processed one at a time so earlier full sources are not evicted before summarization. Every saved memory version and its original owner messages remain retrievable. Summaries are model-authored context; they never confer permissions or establish completion evidence.

The loop requires a memory update before ordinary work or completion when input needs compaction, and at least every 32 model decisions. Source inspection, clarification and explicit blocking remain available at that boundary. This bounds forgotten work without imposing a read count or edit requirement on exploration.

Answered-question previews are bounded to eight recent entries. Raw answers stay in the durable question/message ledgers. Earlier tool receipts and checkpoint decisions have a durable searchable `work` index, independent of the 12 recent context events. Full artifacts remain available through exact same-run handles. Large older mapped results are removed from the working prompt before context overflow; ordinary complete native file reads remain usable. Oversized inline results fall back to inspectable artifacts. An impossibly small configured context pauses for recovery rather than permanently failing the task.

Focused verification:

```powershell
cargo test --offline --lib steering
cargo test --offline --lib memory
cargo test --offline --lib kernel::tests
cargo test --offline --test loop_durability --test user_questions
```

The local HTTP fixture processes five legal 65,536-byte answers with five distinct trailing API constraints, retrieves each full source once, saves each constraint into successive memory, continues discovery, archives history, and reopens the store. It verifies all five constraints survive, prompt growth stays bounded, and there are no context-limit or rejected-action events. The fixture makes no hosted calls. Other tests cover archival migration, delivery races, same-run source isolation, mandatory memory cadence, and old checkpoint retrieval.

## Progress guard

The guard hashes canonical actions and full results locally, ignoring volatile elapsed-time fields. Only digests enter its observation ledger; image changes still count as new information. New searches, files, ranges, results, real workspace revisions and task-owner messages reset stagnation. Reworded checkpoints are not environmental progress. Repeated known outcomes produce strategy feedback; a third identical rejection, or four consecutive steps without new information, pauses the task in durable recovery instead of spending indefinitely. The counters survive process restarts. This is an exact-observation guard, not a claim to recognize every semantic loop.

In the local five-answer fixture, the twelve complete prompts measured 73,329 tokens using the pinned `o200k_base` tokenizer. Reinserting only the old full-answer field twelve times would measure 247,056 tokens, before adding any other prompt content. This is controlled prompt-exposure evidence, not a hosted-model endurance benchmark or a universal savings rate. The fixture enforces a threefold reduction against that reference while preserving all five distinct constraints.
