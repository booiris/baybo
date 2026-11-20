# The ask prelude as a handle

Implemented. `AgentLoop::settle_before_append` returns a `SettledTranscript`
borrowing the loop. Its five input append methods and manual compaction
operate on that loop for the duration of the borrow. A coalesced user batch
shares one handle across its rows. The cron notification acquires its handle
before opening its notification turn.

See [the turn prelude](../modules/human-ask.md#the-turn-prelude-one-home-for-unapplied-asks)
for the ordering guarantee and [the agent entry points](../modules/agent.md#unapplied-asks-the-turn-prelude-and-the-resume-door)
for its callers.
