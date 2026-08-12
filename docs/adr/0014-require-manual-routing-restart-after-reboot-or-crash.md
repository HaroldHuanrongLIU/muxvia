# Require manual routing restart after reboot or crash

The Routing Service will be launched as a detached process and continue after the Control Plane exits, but Muxvia will not install a `launchd` or `systemd --user` unit in the first release. After a machine reboot or Routing Service crash, managed Target CLIs fail closed until the Operator runs the Control Plane again. Startup automatically resumes persisted Target Takeovers whose Managed Configuration has not drifted and routes drifted state into explicit reconciliation; a stopped process cannot inject a custom recovery message into the native CLI error path.

When the last Target Takeover is disabled safely, the Routing Service exits after completing pending control operations and committed streams. The Control Plane starts it again on demand for database access or a new takeover.
