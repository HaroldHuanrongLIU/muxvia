# Retain upstream error payloads in failed request records

Muxvia will match CC-Switch by excluding successful prompt and response bodies from Request Records while allowing failed records to retain the upstream error payload for diagnostics. Because upstream errors can echo sensitive request material, this behavior must be disclosed rather than represented as metadata-only logging.
