# Restore database and managed configuration together

Every restore will use a Recovery Snapshot and treat the SQLite state, the separate Subscription Account credential file, current Target Provider pointers, Target Takeover state, and external Managed Configuration as one recoverable operation. Muxvia intentionally rejects CC-Switch's inconsistent SQL-import and binary-restore outcomes: it will preflight the complete target state and either apply it successfully or roll back to the pre-restore snapshot.
