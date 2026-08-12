# Materialize universal providers per target

Muxvia will treat a Universal Provider as a reusable configuration source and explicitly synchronize it into independent Target Providers for Codex CLI and Claude Code. The Routing Service consumes only Target Providers, preserving each CLI's native protocol and model configuration while still giving the Operator a single place to maintain shared upstream details.
