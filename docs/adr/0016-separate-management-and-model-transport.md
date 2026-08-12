# Separate management and model transport

Control Plane operations will use a private Unix domain socket, while Codex CLI and Claude Code model requests will use authenticated loopback HTTP endpoints required by their provider protocols. Management authority will never be granted by a Target CLI's Routing Credential.
