# Claude Code Codex Subscription Bridge Implementation Plan

**Goal:** Implement GitHub issue #13 as a Claude-only, account-backed Target Provider that converts Anthropic Messages to the pinned ChatGPT Codex Responses behavior and safely participates in Provider failover.

**Architecture:** A closed credentialless Provider declaration consumes T11 binding metadata. A narrow account resolver is installed into Claude model listeners before bootstrap. A private Subscription Bridge adapter owns the fixed endpoint, identity headers, bounded request conversion, streaming response conversion, and cancellation. Existing route-plan health/failover remains the only retry engine.

**Tech stack:** Rust 2024, Tokio, Axum, reqwest/rustls, SQLite, serde/serde_json, futures streams, Bun, TypeScript, Zod, SolidJS/OpenTUI, real UDS/loopback/process harnesses.

## Global Constraints

- Implement issue #13 and the approved T12 design only.
- Test only at the six confirmed seams.
- Use vertical RED→GREEN slices and fixed secret-free diagnostics.
- Preserve the exact production endpoint and source-derived headers; injection is test-only and crate-private.
- Never persist or project an access token.
- Never substitute an account identity or add account-level failover.
- Do not claim unsupported model families or full CC-Switch compatibility.
- Use `apply_patch`, stage exact files, commit locally, and do not push, merge, close #13, or remove the worktree.

## Task 1: Declare the Subscription Bridge Target Provider and migration

**Tracer bullet:** A Claude Target Provider using the Subscription Bridge can be created from the closed preset without a credential, requires Takeover plus a binding, migrates an unchanged v11 database atomically, and round-trips through Rust/Zod/JSON Schema without widening Universal Providers.

1. Add protocol/schema/preset fixture REDs for `codex-subscription`, `codex-subscription-bridge`, fixed base URL, no credential, and Takeover requirement.
2. Add v11→v12 migration REDs preserving all existing table fingerprints and rejecting invalid Bridge rows.
3. Add Provider CRUD/completeness REDs for no binding, fixed/follow binding, redirect attempts, credential attempts, update, duplicate, Direct, and failover plan membership.
4. Implement the minimal closed enum/schema/provider changes.
5. Run provider/protocol/state focused gates, fmt, clippy, typecheck, and diff checks.
6. Commit `feat: declare subscription bridge providers`.

## Task 2: Implement source-derived request conversion

**Tracer bullet:** Synthetic pinned fixtures convert exact Anthropic Messages text/tools into exact Responses JSON and exact source-derived headers for both claimed model names without exposing account or request secrets.

1. Add a fixture manifest naming CC-Switch v3.19.2 and commit `43eaf07355af145aebfee301801779e824d4c221`.
2. RED exact text/system/default fields and removed sampling/output fields.
3. RED tool definitions, tool choice, `tool_use`, `tool_result`, and canonical argument JSON.
4. RED Fixed identity headers and all session extraction precedence cases.
5. GREEN a private adapter with one typed request-conversion entry point.
6. RED malformed/oversized/unsupported input and `/count_tokens`; GREEN fixed local failures.
7. Commit `feat: encode subscription bridge requests`.

## Task 3: Implement streaming response conversion and cancellation

**Tracer bullet:** A chunk-split Responses SSE fixture becomes exact Anthropic text/tool/usage/error frames, and dropping the downstream body drops the upstream stream without a detached task.

1. RED response.created, text block, tool call/arguments, usage, completed/incomplete stop reasons, and ordering.
2. RED failed/error event and non-success HTTP mapping with raw-body scanning.
3. RED arbitrary chunk boundaries, comments, multiple data lines, malformed/contradictory/oversized/incomplete streams.
4. GREEN an owned incremental converter stream.
5. RED cancellation/drop observation and no late output; GREEN without sleeps or detached work.
6. Commit `feat: stream subscription bridge responses`.

## Task 4: Resolve accounts per request and route failover

**Tracer bullet:** Through a real Claude listener, Fixed uses only its exact account, Follow Default follows a changed default on the next request, dangling/Needs Reauthorization/refresh failure advances to the next Provider, and a skipped circuit member never refreshes.

1. Add a crate-private resolver trait and redacted resolved-access value.
2. RED Fixed, Follow Default, default change, dangling Fixed, no default, Needs Reauthorization, transient refresh, permanent rejection, and identity mismatch.
3. GREEN DeviceAuthorizationManager resolution using T11 persistence/cache.
4. RED listener installation before committed Takeover bootstrap and missing-runtime fail-closed behavior.
5. Make route request building async so only admitted Bridge members resolve accounts.
6. RED real loopback endpoint/headers/body plus provider-level failover, route health, serving provider, retryable upstream status, and request pinning.
7. GREEN RouteState/ActivationService integration without changing ordinary Anthropic/Codex paths.
8. Commit `feat: route subscription accounts through claude`.

## Task 5: Add Target workflow and risk disclosure

**Tracer bullet:** The real Claude Target Provider form creates the fixed credentialless preset, requires the existing binding workflow, blocks Direct, and renders exact English/Chinese risk and deviation disclosures at every supported size.

1. RED Target provider catalog/picker/form for the new preset and no credential input.
2. RED missing/dangling/Needs Reauthorization binding presentation and account-overlay handoff.
3. RED fixed base/auth/routing values and redirect/credential mutation attempts.
4. GREEN the smallest Target-only workflow changes.
5. RED exact English/Chinese undocumented-interface, account/quota, continuity, terms, support, tested-model, and Compatibility Deviation copy.
6. GREEN public docs and responsive renderer.
7. Add scan-first controlled credential/account/config/backend/settings mutation proofs.
8. Commit `feat: add subscription bridge workflow`.

## Task 6: Prove the complete Bridge end to end

**Tracer:** With a temporary Muxvia Home, real binary, real UDS, real SQLite/private account file, real Claude listener, deterministic Device Authority/upstream, and OpenTUI renderer:

1. authorize two accounts and create/bind the Target Provider using the Subscription Bridge;
2. activate Takeover and send `gpt-5.6` text plus tool traffic through exact request/stream fixtures;
3. change Follow Default and prove the next request uses the new identity;
4. switch to Fixed, delete that account, and prove only Provider failover occurs;
5. mark Needs Reauthorization, repair the same identity, and prove routing resumes;
6. restart and prove committed Takeover, binding, private permissions, no access token, and exact headers;
7. reject Direct and `/count_tokens` with named deviations;
8. cancel a streaming request and prove upstream ownership drops with no late frames;
9. close all sessions and require natural status 0, UDS removal, and listener shutdown; and
10. scan SQLite, files, frames, activities, Debug surfaces, upstream recordings, and process output before semantic assertions.

Run focused Bridge fixtures/listener/UDS/UI/tracer gates, the complete Rust workspace serialized, full Control Plane, `bun run verify`, fmt, clippy `-D warnings`, typecheck, frozen install, diff, lockfile, and clean-worktree checks. Commit `test: prove subscription bridge end to end`.

## Final Review

Review the complete branch against the T12 base on both approved design/spec and repository standards. Fix every Critical, Important, and accepted Minor through focused RED→GREEN loops. Repeat all gates and leave the worktree clean without pushing.
