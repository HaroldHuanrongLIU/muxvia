# Scan native usage without extending service lifetime

Muxvia incrementally imports Native Usage Records when the Control Plane starts and on explicit refresh. While the Routing Service is already alive for Target Takeover, it also scans every 60 seconds, but scanning never keeps the service alive after the last takeover ends. This preserves Direct Activation usage visibility without reversing the on-demand lifecycle decision.
