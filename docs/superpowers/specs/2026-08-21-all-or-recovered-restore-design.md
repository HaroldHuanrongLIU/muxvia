# All-or-recovered restore design

## Scope and public seams

T18 restores one T17 Recovery Backup through the running Routing Service. The agreed test seams are the closed RPC and diagnostic CLI contract, a real UDS/process restore, and Recovery service fault injection. Provider Configuration Export remains unrelated and non-restorable.

`muxvia backup restore <absolute-path> --acknowledge-replace-current-installation` is the only restore command. The acknowledgement is carried over RPC as the fixed `replace-current-installation` literal so callers cannot bypass the destructive-operation guard accidentally. Success reports the restored snapshot identity, the durable pre-restore Recovery Backup path, resumed Target Takeovers, and restart guidance. Output never contains artifact entries or credentials.

## Preflight

The service acquires the installation-wide Recovery gate, both Target mutation gates, and the Subscription Account mutation gate. Before changing live state it:

1. validates the selected artifact, private mode, hashes, format, and supported database migration path;
2. extracts entries into a private staging directory under Muxvia Home and durably writes them;
3. validates and migrates the staged database, runs SQLite integrity and foreign-key checks, and projects both Target Views, the Universal Provider catalog, and the Subscription Account catalog;
4. validates the account document and exact entry presence/modes;
5. validates both Configuration Homes and rejects managed-file symlinks, unsafe modes, malformed documents, and observable Shadowing Configuration;
6. verifies that required target-directory and database staging writes can complete; and
7. checks restored Target Takeover ports before live state is changed.

The staged, migrated database is the image applied by restore. Preflight failure removes staging and mutates no live file, database, or runtime.

## Apply and rollback

After preflight, the service creates and durably installs a normal T17 Recovery Backup of the current installation. That artifact is retained on success and failure. The service then drains both route runtimes before installing restored state.

Both Target mutation gates and the Subscription Account mutation gate remain held for the complete apply-or-rollback operation, so concurrent service mutations serialize behind Restore. Restore installs the staged SQLite image, Subscription Account JSON, Codex configuration, and Claude settings, then rereads and verifies every surface before releasing those gates. On success it resets publication tracking and bootstraps restored Takeovers through the normal startup path. Existing Target CLI processes are never claimed to hot-reload; the result tells the Operator to start new processes.

Any database, file, process, or verification failure restores every surface from the pre-restore snapshot and verifies it before returning `recovery-backup-restore-failed`. If exact rollback cannot be verified, the service records installation-wide Recovery Required for both Targets and Subscription Accounts, returns `recovery-backup-recovery-required`, and identifies the retained pre-restore Recovery Backup in a safe diagnostic. It never reports success or a clean state after an unverified rollback.

## Crash boundary

The pre-restore Recovery Backup is the durable manual recovery point. T18 coordinates handled failures in one running process; a power loss during multi-file installation may require the Operator to invoke restore again from that artifact. The CLI calls this out without exposing private contents.
