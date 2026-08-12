# Protect generated target provider lifecycles

A Generated Target Provider carries explicit Universal Provider and Target CLI provenance and cannot be deleted independently. Duplicating it creates an ordinary detached Target Provider. Deleting its Universal Provider or disabling its target is blocked while the generated record is Current or referenced by an Activated Route Plan, and the Control Plane lists the references that must first be removed. This intentionally rejects CC-Switch `v3.19.2`'s unchecked child deletion path.
