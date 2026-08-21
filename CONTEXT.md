# Muxvia

Muxvia is a terminal-native control plane for managing model access for AI coding command-line tools. Its first release serves one local operator on macOS or Linux and targets Codex CLI and Claude Code.

## Language

**Operator**:
The single local person who configures and controls Muxvia and its managed coding tools.
_Avoid_: User account, administrator

**Target CLI**:
An external AI coding command-line tool whose provider, model, routing, health, and configuration state Muxvia manages. The first-release Target CLIs are Codex CLI and Claude Code.
_Avoid_: Agent, client, target application

**Configuration Home**:
The default global configuration directory Muxvia supports for one Target CLI in the first release: `~/.codex` for Codex CLI and `~/.claude` for Claude Code. A directory symlink is canonicalized, while a symlinked managed configuration file is rejected rather than followed or replaced.
_Avoid_: Target Instance, project configuration, Muxvia Home

**Shadowing Configuration**:
A higher-priority Target CLI configuration source that Muxvia does not manage but that may override Managed Configuration, such as a Codex profile or command-line override, or Claude Code managed, command-line, project, or local settings.
_Avoid_: Configuration Drift, invalid configuration, Managed Configuration

**Control Plane**:
The terminal interface through which the Operator manages Universal Providers, Target Providers, models, routes, health, and Muxvia-owned configuration without hosting Target CLI sessions. It presents one Target CLI as the current working context rather than organizing the product as an administrative dashboard.
_Avoid_: Agent UI, terminal wrapper, session host

**Control Plane Command**:
A named Control Plane action whose availability and key bindings are resolved through the centralized layered keymap rather than by independent global component handlers.
_Avoid_: shell command, raw key event, Target CLI command

**Universal Provider**:
A reusable source definition for an upstream model service that can be explicitly synchronized into one or more Target Providers.
_Avoid_: Shared route, global provider, provider group

**Target Provider**:
A Target CLI-specific provider record materialized from a Universal Provider or created directly, containing the native model, protocol, and configuration shape used for routing and activation.
_Avoid_: Universal Provider, account, preset

**Incomplete Provider**:
A saved Provider whose structure is valid but which lacks one or more values required for activation or membership in an Activated Route Plan.
_Avoid_: Invalid provider, disabled provider, draft form

**Target Overlay**:
The Target CLI-specific portion of a Provider configuration that supplements typed canonical fields without overriding fields owned by a Universal Provider.
_Avoid_: Raw config, child override, generated config

**Generated Target Provider**:
A Target Provider owned and materialized by one Universal Provider; it cannot be deleted independently, though it may be duplicated into an ordinary detached Target Provider.
_Avoid_: Child provider, copied provider, preset

**Routing-required Target Provider**:
A Target Provider whose authentication, protocol conversion, subscription access, failover, or other routing behavior cannot work through Direct Activation and therefore requires Target Takeover.
_Avoid_: Proxy provider, unsupported provider, bridge provider

**Current Target Provider**:
The single Target Provider explicitly selected as the primary provider for one Target CLI, recorded authoritatively by the Routing Service; failover does not change it.
_Avoid_: Default provider, active global provider, observed provider

**Serving Provider**:
The Target Provider that most recently served or is currently serving a routed request, which may differ from the Current Target Provider because of failover.
_Avoid_: Current Target Provider, default provider, active snapshot

**Activated Snapshot**:
The immutable provider, model, and routing configuration currently applied to one Target CLI; provider edits and Provider Synchronization do not change it until an explicit Apply or Activate operation succeeds.
_Avoid_: Current Target Provider, live provider record, Recovery Snapshot

**Provider Preset**:
A copy-on-create template used to initialize a new Universal Provider or Target Provider without retaining ownership of the created record.
_Avoid_: Built-in provider, managed template, default provider

**Provider Synchronization**:
The explicit, transactional materialization of one Universal Provider into its enabled Target Providers without changing an Activated Snapshot or Managed Configuration.
_Avoid_: Activation, live update, config write

**Routing Plane**:
The Muxvia capability set that mediates model requests, selects an upstream Target Provider, tracks route health, and applies failure handling.
_Avoid_: Switcher, forwarding rule, proxy feature

**Routing Service**:
The local, long-lived process that keeps the Routing Plane available independently of the Control Plane.
_Avoid_: TUI backend, helper, daemon

**Target Route State**:
The current provider selection, takeover status, failover order, and route health owned independently by one Target CLI.
_Avoid_: Global route, shared active provider

**Failover Chain**:
The ordered Target Providers that the Routing Plane may attempt for one Target CLI before the first valid output is committed to that CLI.
_Avoid_: Load-balancing pool, fallback model, retry list

**Activated Route Plan**:
An immutable, atomically validated snapshot of a Target CLI's applied Failover Chain and member provider snapshots; each request remains pinned to the plan epoch under which it began.
_Avoid_: Failover Chain draft, provider order, Current Target Provider

**Route Health**:
The passive assessment of a Target Provider derived from real routed requests and failure-handling state.
_Avoid_: Reachability, uptime, provider status

**Reachability Check**:
An Operator-initiated connectivity probe that reports whether a Provider endpoint can be reached without changing Route Health.
_Avoid_: Health check, model test, circuit-breaker probe

**Routing Credential**:
A machine-local secret placed in Managed Configuration so a Target CLI can authenticate its requests to the Routing Service.
_Avoid_: Provider API key, admin token, subscription token

**Credential Reference**:
A Provider's reference to one stored credential identity; duplicating a Provider may reuse the reference but never copies secret bytes as runtime state.
_Avoid_: Routing Credential, copied secret, API key value

**Managed Configuration**:
The exact subset of a Target CLI's global configuration whose lifecycle Muxvia owns, including the prior values needed to detect drift and restore the pre-Muxvia state.
_Avoid_: Full config, generated config, settings snapshot

**Configuration Drift**:
A difference between the currently observed Managed Configuration and the state Muxvia last applied or adopted.
_Avoid_: Corruption, invalid config

**Target Takeover**:
The state in which a Target CLI's Managed Configuration directs its model requests through the local Routing Service.
_Avoid_: Provider activation, proxy mode, global takeover

**Direct Activation**:
The application of a Current Target Provider directly to a Target CLI's Managed Configuration while Target Takeover is disabled.
_Avoid_: Provider synchronization, hot switch, direct route

**Model Discovery**:
The retrieval of model identifiers offered by a Target Provider for selection, without changing the Current Target Provider or Managed Configuration.
_Avoid_: Model sync, health check, activation

**Imported Current**:
A Target Provider created during migration from a distinct live Target CLI configuration, without selecting it as the Current Target Provider or changing the observed configuration.
_Avoid_: Current Target Provider, migration default, adopted config

**Request Record**:
A persisted account of a routed request's provider, model, usage, cost, latency, outcome, and, for failed requests, at most 64 KiB of upstream error payload plus an explicit truncation marker.
_Avoid_: Transcript, session, trace

**Native Usage Record**:
Usage metadata imported incrementally from a Target CLI's own local session log rather than observed by the Routing Plane.
_Avoid_: Request Record, transcript, routed request

**Migrated Usage Record**:
Usage metadata imported from an Operator-selected external product export, retaining its source provenance without claiming that Muxvia routed or natively observed the request.
_Avoid_: Request Record, Native Usage Record, imported transcript

**Daily Usage Rollup**:
The aggregate counts, tokens, costs, and latency retained for a completed local calendar day after older detailed usage records are pruned.
_Avoid_: Request Record, backup, billing statement

**Pricing Snapshot**:
The immutable unit prices, source model, multipliers, and pricing time that make one recorded cost estimate reproducible; an initially unpriced record freezes its snapshot on its first successful backfill.
_Avoid_: Current model price, invoice, pricing preset

**Codex Subscription**:
A ChatGPT-backed identity and entitlement used to access Codex, distinct from an OpenAI API credential.
_Avoid_: OpenAI API key, Codex API key

**Subscription Account**:
The locally stored authorization and account metadata for one Codex Subscription, which may be selected as the default or bound to a Target Provider and may enter a persistent Needs Reauthorization state.
_Avoid_: Provider, API account, API key

**Device Authorization**:
The Compatibility Baseline-derived login flow in which Muxvia displays and attempts to copy a short user code, opens a remote verification page on a best-effort basis, polls remotely, and receives the code verifier from the service without running a local browser callback listener.
_Avoid_: native Codex login import, loopback callback, pasted refresh token

**Follow Default**:
A dynamic Subscription Account binding that resolves the current global default account for every request rather than capturing the account selected when a Provider was saved.
_Avoid_: Fixed binding, account failover, copied account

**Subscription Bridge**:
The capability that makes model access backed by a Codex Subscription available to Claude Code.
_Avoid_: Account switch, API-key conversion

**Compatibility Baseline**:
The pinned stable CC-Switch source revision whose observable Codex CLI and Claude Code routing behavior Muxvia claims to reproduce.
_Avoid_: Latest main, upstream dependency, feature checklist

**Compatibility Deviation**:
An intentional, documented difference from the Compatibility Baseline rather than an accidental incompatibility.
_Avoid_: Bug, improvement, unsupported behavior

**Recovery Snapshot**:
A point-in-time record of product-owned database state, auxiliary credential state, and Managed Configuration sufficient to restore them to one consistent state.
_Avoid_: Database backup, config backup, export

**Recovery Backup**:
A private, restorable artifact containing a Recovery Snapshot, including provider and Subscription Account credentials, intended for the same Operator rather than for sharing.
_Avoid_: Provider Configuration Export, database dump, public export

**Provider Configuration Export**:
A shareable, always-redacted artifact containing declarative Provider configuration without provider secrets, subscription tokens, Routing Credentials, or other restorable private state.
_Avoid_: Recovery Backup, Recovery Snapshot, credential export

**Import Provenance**:
The source product, source target, source identifier, and normalized configuration fingerprint retained for an imported record without reusing its identity.
_Avoid_: Provider ID, display name, migration status

**Muxvia Home**:
The single `~/.muxvia` directory tree containing all Muxvia-owned configuration, state, credentials, logs, backups, runtime metadata, and local service endpoints in the first release.
_Avoid_: Configuration Home, current directory, platform application-support directory

**Release Bundle**:
One versioned installation unit containing the public `muxvia` executable, its private `muxvia-routing` sidecar, and a manifest that binds their product version, RPC protocol version, and integrity hashes. Its members are never installed or upgraded independently.
_Avoid_: npm package, Routing Service process, source checkout

**Pricing Catalog**:
The versioned model-price input used only to create new Pricing Snapshots or to perform the one permitted backfill of an unpriced record.
_Avoid_: Pricing Snapshot, invoice, live billing rate

**Target Compatibility Probe**:
A read-only check of an installed Target CLI's version and required configuration capabilities that classifies it as tested, unknown-but-compatible, or incompatible before Muxvia writes Managed Configuration.
_Avoid_: Reachability Check, Model Discovery, strict version allowlist
