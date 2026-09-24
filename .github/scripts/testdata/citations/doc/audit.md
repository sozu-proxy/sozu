# Audit fixture — two citations that must be flagged, two that must not

`Ledger::retire_entry` drops the entry at `audit.rs:10`. That citation is wrong
and the audit must say so: the prose names a call and the line is the comment
sitting above it.

`Ledger::settle_fee` is applied at `audit.rs:11`. That one is wrong the other
way round — the line is ordinary code, so nothing about its shape is suspect,
and only the absent name gives it away.

`Ledger::retire_entry` covers the comment introducing it (`audit.rs:10-13`),
which is how a range names a branch here and must never be flagged.

`recount_all` totals the three counters at `audit.rs:30`, a line far enough
below its own declaration that only the enclosing item answers for it. It must
never be flagged either.
