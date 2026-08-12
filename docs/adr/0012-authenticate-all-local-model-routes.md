# Authenticate all local model routes

Loopback binding alone does not distinguish the managed Target CLIs from other local processes, so every Muxvia model route will validate a generated Routing Credential injected through Managed Configuration. Management operations will use a separate local control credential or stronger operating-system-local channel so model clients do not gain administrative authority.
