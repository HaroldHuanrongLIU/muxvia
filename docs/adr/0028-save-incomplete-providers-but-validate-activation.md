# Save incomplete providers but validate activation

Muxvia will represent Provider configuration as typed canonical fields plus a constrained Target Overlay. Saving requires structural and safety validity but may produce an Incomplete Provider. The Routing Service performs complete target, credential, model, and protocol validation before activation or inclusion in an Activated Route Plan. A Target Overlay cannot replace fields owned by its Universal Provider, preserving explicit field ownership while allowing configuration to be completed incrementally.
