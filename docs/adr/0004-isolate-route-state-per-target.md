# Isolate route state per target CLI

Codex CLI and Claude Code will each own independent current-provider selection, takeover state, failover order, Route Health, and circuit-breaker state. Universal Provider synchronization may share configuration inputs, but switching or failing over one Target CLI must never change the other's live route.
