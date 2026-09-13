# Workflow implementation notes

These notes describe the current implementation, not a claim of full Everything compatibility. See the [project overview](../README.md) for setup and limitations.

## File actions, query leases, and updates

Bulk actions resolve selected rows against one retained snapshot before presenting a per-file review. Rename rules, destination conflicts, skipped files, and partial completion are shown explicitly. Operation history records intents and completed changes; hard-link aliases share verified identity updates so a batch and its undo do not mistake their own renames for external edits. Changed files still require review rather than automatic undo.

The review binds both source and destination directory identities. Execution and undo use open directories and verified file handles, so a changed ancestor cannot redirect an operation. Moves first capture the entry in private storage on the same volume, verify the captured identity, and publish exclusively. Native Trash receives only that protected path. A failed rollback leaves a recorded recovery path; it never overwrites a newly occupied source name. If no private workspace with trusted ancestry is available, the action fails before mutation. Regular-file copies attempt an APFS clone before descriptor-based copying.

All application-owned protocols and persistent formats start at version 1. Operation history accepts only explicit format-1 records with complete source and destination parent identities and paths; missing fields, unversioned records, and other versions cannot authorize undo. The earlier JSON journal is not loaded. The app starts with `APFSearch/v1` data and `APFSearch.v1.` preferences, without importing, converting, or deleting earlier development data. SQLite accepts only a blank database or the APFSearch format-1 application marker, and prepared snapshots use a single `APFIDX01` layout.

Window queries share immutable snapshot data and have a separate budget of 128 leases, including replacement pages awaiting adoption. Eight additional leases remain available for concurrent exports and other operations. Closing windows, discarding replies, and finishing operations release their leases; abandoned leases expire after five minutes without renewal.

Configured updates keep one download or Installer session active at a time. Cancelled packages are removed immediately; opened packages remain available until Installer exits. If APFSearch exits first, a later launch or activation reclaims its abandoned download directories once Installer is no longer running.

## Index scope and file-list imports

A fresh root selection may resolve an intermediate directory alias once. Persisted roots and subsequent filesystem events retain that selected scope. Metadata access rejects intermediate symlinks; a final symlink remains its own searchable entry. Verified system aliases and System/Data firmlink mapping retain their normal behavior.

CSV file lists are decoded incrementally as UTF-8 and staged in bounded SQLite batches. The completed import replaces the list atomically and publishes one search snapshot. Cancellation, malformed input, or a limit failure leaves the previous list intact; a newly imported list is not registered until completion.

Import limits are 1 GiB of source CSV, 2,000,000 data records, 256 columns, 64 KiB per decoded field, 1 MiB per record, and 256 MiB of stored path/name/extension text. Swift-to-core batches hold at most 4,096 rows and 1 MiB of text. Exceeding a limit produces an explicit error instead of truncating the list. File lists must be regular files and must remain unchanged while being read.
