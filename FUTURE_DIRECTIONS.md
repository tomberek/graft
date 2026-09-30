# Future directions

Speculative architectural ideas that are out of scope for now — recorded so
the reasoning behind them isn't lost, not because they're scheduled work.

## Hiding grafting inside a dynamic derivation (`builder-rpc-v0`)

**Status: investigated, not started.** This is about a fundamentally
different execution model for graft's core substitution logic — running it
*as a Nix derivation* (cacheable, substitutable, remote-buildable,
integrated into the normal build graph) instead of as an out-of-band CLI
that shells out to `nix build` in a loop. Findings below are grounded in a
local checkout of Determinate Systems' Nix fork at `~/nix`; `builder-rpc-v0`
does not appear to exist in upstream CppNix or in Lix, so anything built on
it is tied to that fork.

### Why graft can't just run inside an ordinary sandbox

Confirmed directly in `~/nix/src/libstore/daemon.cc:330-356`: a restricted
daemon socket exposed to a sandboxed builder (used by both `recursive-nix`
and `builder-rpc-v0`) only permits an explicit allowlist of worker-protocol
operations: `AddToStore`, `AddMultipleToStore`, `AddToStoreNar`,
`AddToStoreScanning`, `SubmitOutput`, `AddTempRoot`, `IsValidPath`. Nothing
else — confirmed against the full op enum in `worker-protocol.hh`, which
also defines `BuildPaths`, `BuildDerivation`, `BuildPathsWithResults`,
`EnsurePath`, and the various `Query*` ops, none of which are in the
allowlist. Graft's two load-bearing primitives — walking the reference graph
(`nix path-info`) and asking Nix to build a synthetic/edited derivation
(`nix build`) — are both excluded by design. `builder-rpc-v0` additionally
requires the derivation to be content-addressed
(`unix-derivation-builder.cc:356`) and, when set, the builder receives *no*
`$out`-style environment variables at all — it's expected to submit
finished output content directly via `AddToStore*`/`SubmitOutput`.

### The architecture that does work

Three existing Nix mechanisms compose to sidestep both restrictions
entirely, without needing anything from the disallowed op set:

1. **Closure/reference data in, without any query.**
   `exportReferencesGraph` (a completely ordinary derivation attribute, used
   by e.g. `pkgs.closureInfo`) computes closure/reference info from the
   daemon's own store metadata *before* the sandbox starts, and writes it as
   a plain file (`derivation-env-desugar.cc:51-56`,
   `store.exportReferences(storePaths, inputPaths)`). No query op, no build,
   no IFD. One real constraint: `store-api.cc:850-859` requires every
   exported root path to already be in the calling derivation's own input
   closure — natural for graft's use case, since you'd declare the
   pre-graft closure as an input anyway.

2. **A graft/rebuild plan, out, as a chain of ordinary `.drv`s.**
   `outputHashMode = "text"` + `__contentAddressed = true` lets a
   derivation's output *be* a `.drv` file (the `dynamic-derivations`
   experimental feature). `tests/functional/dyn-drv/non-trivial-submitted.nix`
   is close to a reference implementation: a `builder-rpc-v0` derivation
   constructs a chain of `.drv`s inline via `nix derivation add` (each
   referencing the previous through `inputs.drvs`), never builds any of
   them, and calls `nix store submit-output <last-drv-path> out` to hand
   the chain's tail to the daemon as its own output.
   `tests/functional/dyn-drv/recursive-mod-json.nix` is graft's own
   `edit_drv.rs` algorithm almost verbatim, already in Nix's test suite:
   `nix derivation show | jq edit | nix derivation add`, submitted as the
   dynamic output.

3. **Resolution happens outside the restricted socket.**
   The outer expression does `builtins.outputOf plannerDrv.outPath "out"`,
   and Nix's ordinary, *unrestricted* build scheduler resolves and builds
   the emitted chain — with normal substitution, caching, parallelism, and
   remote-builder eligibility. The expensive part (potentially hundreds of
   grafts) is never inside `builder-rpc-v0`'s restricted socket at all; only
   the cheap planning step is.

Concretely: a planner derivation reads the pre-graft closure's registration
data (piece 1), runs graft's existing `would_change`/`rewrite_one` walk
logic against it instead of live `nix path-info` calls, and instead of
calling `nix build` for each affected node, constructs that node's `.drv`
inline (exactly today's `derivation::add_with_retry`, just relocated) and
chains it to the previous one. The final chain tail becomes the planner's
dynamic output; Nix's normal derivation resolution does the actual grafting
as ordinary, cacheable builds.

### Caveats found during investigation

- **Fork-specific.** No evidence `builder-rpc-v0` exists outside this
  Determinate Systems checkout.
- `AddToStoreScanning` requires the `dynamic-derivations` experimental
  feature specifically (`daemon.cc:1073`), not just `recursive-nix` —
  so the full flag set needed is `ca-derivations dynamic-derivations`.
- `SubmitOutput` currently only accepts a concrete (`Opaque`) store path,
  not forwarding another derivation's future dynamic output as your own
  (tracked upstream as `NixOS/nix#12727`). Not a blocker here — the planner
  always submits its own freshly-added, already-concrete `.drv`.
- `exportReferences` on a graph containing CA derivations isn't implemented
  yet (`store-api.cc:879`) — relevant only if a node in the closure being
  exported is itself content-addressed.
- Dynamic-derivation graph resolution isn't proven to terminate in general
  (`doc/manual/source/store/resolution.md:177`) — theoretical, not a
  practical concern for a finite closure.
- The IPC output-submission mechanism doesn't support self-references in
  general (`doc/manual/source/store/building.md`) — shouldn't matter for
  chaining distinct `.drv`s, but worth keeping in mind.

### If this gets picked up later

The planner's builder could plausibly *be* the `graft` binary itself,
cross-compiled and invoked as the derivation's builder, with two internal
changes: swap `store::closure`/`store::references` (live `nix path-info`)
for reading the `exportReferencesGraph`-supplied registration file, and swap
`derivation::realise` (`nix build`) for chaining the next `.drv` via
`inputs.drvs` instead of building it. Everything else — `would_change`,
`rewrite_one`, `substitute_dependency`, `add_with_retry` — carries over
close to unchanged.
