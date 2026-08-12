# Freeze auditable pricing snapshots

Every nonzero cost estimate carries an immutable Pricing Snapshot containing the applied unit prices, pricing model or source, multipliers, and pricing time. A record created without a known price may be backfilled once and freezes its snapshot at that first successful backfill; later catalog changes never silently recalculate it. This intentionally adds reproducibility beyond CC-Switch `v3.19.2` while retaining its estimated-cost status.

The first release ships a Pricing Catalog pinned to each Muxvia release and offers an explicit Operator action to fetch a newer catalog from models.dev. Muxvia performs no background pricing update. A catalog update affects only future Pricing Snapshots and the first permitted fill of still-unpriced records; it never rewrites a frozen snapshot.
