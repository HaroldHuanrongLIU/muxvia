# Own only managed target configuration fields

Muxvia will modify only the global Target CLI configuration fields required for provider and routing activation, while recording prior values for preview, drift detection, and exact restoration. This rejects whole-file snapshot replacement because unrelated settings such as permissions, hooks, and MCP configuration remain owned by the Operator or other tools, even though field-level ownership requires merge and concurrency handling.
