# Keep product state under one Muxvia home

The first release stores all Muxvia-owned configuration, SQLite state, private credential files, request and usage data, logs, backups, runtime metadata, Unix sockets, and installation metadata under the single `~/.muxvia` tree instead of splitting them across platform-native macOS and XDG locations. `muxvia paths` reports every effective path. Private subtrees and files receive best-effort restrictive Unix permissions, while backups and shareable exports remain explicitly different artifact types.
