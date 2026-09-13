# Workflow implementation notes

These notes describe the current implementation, not a claim of full Everything compatibility. See the [project overview](../README.md) for setup and limitations.

## File actions, query leases, and updates

Bulk actions resolve selected rows against one retained snapshot before presenting a per-file review. Rename rules, destination conflicts, skipped files, and partial completion are shown explicitly. Operation history records intents and completed changes; hard-link aliases share verified identity updates so a batch and its undo do not mistake their own renames for external edits. Changed files still require review rather than automatic undo.

Window queries share immutable snapshot data and have a separate budget of 128 leases, including replacement pages awaiting adoption. Eight additional leases remain available for concurrent exports and other operations. Closing windows, discarding replies, and finishing operations release their leases; abandoned leases expire after five minutes without renewal.

Configured updates keep one download or Installer session active at a time. Cancelled packages are removed immediately; opened packages remain available until Installer exits. If APFSearch exits first, a later launch or activation reclaims its abandoned download directories once Installer is no longer running.
