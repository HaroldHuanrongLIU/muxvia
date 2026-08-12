# Use one routing credential per target CLI

Codex CLI and Claude Code will each receive a distinct Routing Credential for their local model routes. Credentials remain stable across Target Provider switches so failover and hot switching do not rewrite client configuration, while compromise or takeover removal for one Target CLI does not authorize the other.
