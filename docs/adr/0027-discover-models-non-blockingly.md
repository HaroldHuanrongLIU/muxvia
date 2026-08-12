# Discover models non-blockingly

Opening a Target Provider editor starts one asynchronous Model Discovery using the last-saved endpoint and credential. Discovery never blocks manual model entry; failure leaves the editor usable. Changes to unsaved endpoint or credential fields do not trigger background requests and require an explicit refresh, preventing repeated credential-bearing probes while the Operator types. This is an intentional Compatibility Deviation from CC-Switch `v3.19.2`'s manual-only discovery.
