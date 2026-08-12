# Support only default target configuration homes

The first release manages only `~/.codex` for Codex CLI and `~/.claude` for Claude Code and will neither register nor scan additional Configuration Homes. A nondefault `CODEX_HOME` or `CLAUDE_CONFIG_DIR` detected in the Control Plane environment is reported as unsupported rather than silently managed as the default target.

Muxvia owns only the default global base or user layer. It detects known Shadowing Configuration where observable, identifies the source, and warns that effective routing is not guaranteed, but does not edit Codex profiles or command-line overrides, Claude Code managed, command-line, project, or local settings. A Configuration Home symlink is canonicalized; if `config.toml` or `settings.json` itself is a symlink, managed writes are blocked until the Operator replaces or resolves it outside Muxvia. This preserves the global-only scope and avoids file-symlink escape and replacement behavior.
