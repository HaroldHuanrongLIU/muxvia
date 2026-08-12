# Retain dynamic subscription account bindings

Muxvia retains CC-Switch `v3.19.2` Subscription Account semantics: one global default, fixed Provider bindings, and dynamic Follow Default bindings resolved on every request. Changing the default previews affected Providers and takes effect on their next request. Deleting an account is nevertheless allowed even when fixed bindings still reference it, intentionally accepting baseline-compatible dangling bindings that fail authentication and then participate in normal Provider-level failover; no automatic account-level failover occurs.
