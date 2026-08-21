# Private Recovery Backup design

## Scope

T17 creates and inspects one private Recovery Backup. Restore remains T18. A Recovery Backup is installation-wide and is never a mode of the shareable Provider Configuration Export.

## Coordinated Recovery Snapshot

Creation acquires both Target mutation gates and the Subscription Account mutation gate in a fixed order. While those gates are held it:

1. reads the exact Subscription Account JSON and both Managed Configuration files through the existing no-follow file boundary;
2. creates one SQLite online backup from the Routing Service connection;
3. re-reads the external files and rejects the snapshot if any identity or bytes changed; and
4. releases the gates only after the private artifact has been durably installed.

The SQLite backup is a point-in-time database image and already contains Provider credentials, Routing Credentials, Current and Takeover state, Activated Snapshots, and Managed Configuration recovery intents. The separately captured files complete the Recovery Snapshot.

## Private format

The artifact begins with the fixed `MUXVIA-RECOVERY-V1` magic, followed by one bounded, closed JSON manifest and four raw entries in a fixed order:

- coordinated SQLite state;
- Subscription Account JSON;
- Codex CLI Managed Configuration; and
- Claude Code Managed Configuration.

The manifest records the format and database schema versions, snapshot identity, creation time and release, sensitivity label, and each entry's presence, mode, byte length, and SHA-256. Entry contents are streamed rather than embedded in JSON. Inspection validates the entire container, every length and hash, private file permissions, and current compatibility without decoding or returning secret-bearing entry contents.

## Atomic durability

Backups live only under the private `Muxvia Home/backups` directory. Creation writes a private hidden temporary file, flushes and syncs it, atomically renames it to a new `.muxvia-recovery` name, verifies the installed identity, and syncs the directory. Failures before the rename remove the temporary file. A crash can therefore leave a hidden staging file, but never a valid-looking partially written Recovery Backup. Creation is read-only with respect to live product state.

## Operator surface

`muxvia backup create` asks the running Routing Service to create the coordinated artifact. `muxvia backup inspect <absolute-path>` asks it to validate an operator-selected artifact. Human and JSON output always state that the artifact is sensitive and expose only safe metadata. Provider Configuration Export remains target-scoped, shareable, and always redacted; neither surface accepts a secret-inclusion switch.
