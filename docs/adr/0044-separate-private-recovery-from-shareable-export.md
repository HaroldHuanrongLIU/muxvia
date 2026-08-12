# Separate private recovery from shareable export

A Recovery Backup contains the complete Recovery Snapshot needed to restore one Operator's installation, including Provider credentials, Subscription Account refresh tokens, Routing Credentials, and Managed Configuration recovery data. It is created with restrictive local permissions and is always presented as sensitive.

A Provider Configuration Export is a different, shareable artifact and is always redacted. It may include declarative Provider fields, models, ordering, and non-secret routing structure, but never secret bytes, subscription tokens, Routing Credentials, or other private recovery state. The first release offers no switch that turns a Provider Configuration Export into a secret-bearing export.
