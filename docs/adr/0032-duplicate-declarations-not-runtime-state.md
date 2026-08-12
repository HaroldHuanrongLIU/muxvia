# Duplicate provider declarations, not runtime state

Duplicating a Provider creates a new identity and copies its declarative fields, models, Target Overlay, and non-runtime metadata. It may explicitly reuse the same Credential Reference, but it does not copy secret bytes, current selection, Route Health, Activated Snapshot, Activated Route Plan membership, or other runtime state. This gives duplication predictable configuration semantics without creating a second hidden routing identity.
