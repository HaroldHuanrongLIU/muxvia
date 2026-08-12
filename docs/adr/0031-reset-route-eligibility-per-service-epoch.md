# Reset route eligibility per service epoch

Each Routing Service epoch starts with fresh circuit-breaker eligibility and counters. Historical Route Health remains visible after restart only as `Stale/Unknown since restart` and does not influence routing until new requests establish current evidence. This intentionally avoids CC-Switch `v3.19.2`'s split in which persisted health can appear current after its in-memory breaker state has been reset.
