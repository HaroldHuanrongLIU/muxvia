# Store subscription accounts in a private JSON file

Subscription Account metadata, refresh tokens, and the default account identifier will remain in a separate atomically written `0600` JSON file compatible in shape and lifecycle with the pinned bridge behavior; access tokens remain memory-only. A permanently rejected refresh token persists the account as Needs Reauthorization without deleting it or switching identities automatically. The separate file is included with SQLite and Managed Configuration in every Recovery Snapshot and unified restore.

Persisting Needs Reauthorization is an intentional Compatibility Deviation: CC-Switch `v3.19.2` retains the account after refresh rejection but continues to report it authenticated and retries on later requests. Muxvia retains the same identity and no-auto-switch rule while exposing the failure honestly and suppressing futile silent retries until the Operator reauthorizes it.
