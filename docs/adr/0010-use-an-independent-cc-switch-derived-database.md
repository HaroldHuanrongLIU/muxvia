# Use an independent CC-Switch-derived database

Muxvia will maintain its own SQLite database and carry forward only the CC-Switch tables and migrations needed for Codex CLI, Claude Code, Universal Providers, routing, health, request records, usage, and cost. It will neither require nor write the CC-Switch application database, preserving independent installation while reducing adaptation around reused Rust behavior.
