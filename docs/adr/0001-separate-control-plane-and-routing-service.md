# Separate the control plane from the routing service

Muxvia will use a terminal Control Plane that manages state but never hosts Codex CLI or Claude Code sessions, plus a separate local Routing Service that remains available after the Control Plane exits. This split lets native Target CLI sessions run independently in other terminals while preserving hot routing and failover, at the cost of explicit lifecycle, recovery, and version-coordination responsibilities between the two processes.
