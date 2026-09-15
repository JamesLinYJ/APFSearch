# Storage growth

SQLite is authoritative. File paths are unique, snapshot journals deduplicate IDs,
and the prepared-cache journal is bounded at 65,536 IDs. A prepared-cache manifest
is atomically replaced and references immutable sections; existing section files
are never modified or truncated in place. Publication reuses unchanged sections
and collects obsolete sections only after active snapshots release their pins.
Search history retains 100 entries in
the UI; file-operation history deliberately records user operations for undo.

## WAL allocation

Each IndexStore connection sets SQLite journal_size_limit to 16 MiB. SQLite can
exceed this while a transaction or reader needs recovery records. On normal WAL
reset it discards excess spare allocation. The default automatic checkpoint
cadence remains unchanged; no timer, forced checkpoint, vacuum, or extra metadata
write is introduced. The setting is connection-local and applied on every open,
including existing format-1 databases. It does not require index rebuilding.

This bounds retained spare space, not peak transaction size or WAL growth behind
a long-running reader. Never delete WAL/SHM files while the service is running.

## Database free pages

Deleted rows leave reusable free pages in the main database. File size reflects
its historical allocation and need not decrease after deletion. Subsequent writes
reuse those pages. APFSearch does not vacuum the entire index periodically: that
would rewrite live data and can increase disk traffic. Free-page allocation is
not a leak or additional indexed files.

Regression coverage in storage_growth_tests checks existing WAL peak reclamation,
reader snapshot safety, unchanged checkpoint cadence and no writes on reopening,
and stable page allocation across repeated delete/insert cycles. Fixtures do not
establish a whole-volume storage bound or a zero-write guarantee.
