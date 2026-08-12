# Reconcile drift before managed writes

When Muxvia detects Configuration Drift, the Routing Service may continue serving the last loaded Activated Snapshot, but the Control Plane blocks provider save or activation, Provider Synchronization, and restore writes for that Target CLI until the Operator explicitly chooses Adopt, Reapply, or Restore. Editors continue to show saved provider state rather than silently loading live configuration. This intentionally rejects CC-Switch `v3.19.2`'s direct-mode behavior of silently adopting live changes while editing or switching.
