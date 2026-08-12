# Make the routing service the database owner

The Routing Service will be the sole process that opens and migrates the Muxvia SQLite database. The Control Plane will perform provider, routing, health, log, usage, and configuration operations through local RPC, preventing two independently versioned processes from competing over transactions, migrations, or live route state.
