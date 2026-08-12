# Notify about updates without telemetry or auto-install

The Control Plane checks the public release manifest at most once per day by default and notifies the Operator when a newer version exists; the check can be disabled. It never installs an update automatically. An explicit update action delegates Homebrew and npm installations to their owning package manager and applies the complete, verified Release Bundle atomically only for a Muxvia-managed verified-download installation.

Muxvia sends no product telemetry, analytics, crash reports, configuration, or usage records. Diagnostics remain local unless the Operator deliberately copies or exports them. The first release supports English and Simplified Chinese through an internationalized message catalog and does not promise the wider CC-Switch locale set.
