# Use one current provider per target

The Routing Service database will hold one authoritative Current Target Provider for Codex CLI and one for Claude Code. Muxvia intentionally omits CC-Switch's device-local-current plus database-fallback pairing because the first release has one local database and no cloud synchronization, eliminating conflicting current-provider sources.
