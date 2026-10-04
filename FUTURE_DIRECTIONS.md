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

## Replacing by package name instead of exact store path

**Status: designed, not started.** §8 of DESIGN.md let `--override` accept
installables instead of requiring a pre-built store path, but that still
requires knowing *which* installable produces the exact thing already in
the target closure. Guix users never see a hash at all — grafting is driven
by a `replacement` field on a package. The natural next ergonomic step is
letting `--override` accept a bare package name and have graft find the
right store path inside the closure itself.

### Why naive "match by name, replace every match" is unsafe

Two real complications, not just theoretical ones:

- **Duplicates at genuinely different versions.** A closure can easily
  contain both `openssl-1.1.1w` and `openssl-3.2.1` at once (something
  pins the old one for compatibility while everything else moved on). If a
  name search for `openssl` found both and replaced them both with the same
  `new` value, that would silently apply a version-3 security fix to
  whatever specifically needed version 1.1.1 — a much bigger, likely-wrong
  change than the user asked for. "Replace every match" is only safe when
  there's exactly one distinct match; in the general case it isn't.
- **Wrappers don't hide the real dependency, but they do add noise.**
  Worth being precise about what wrappers actually threaten here: a wrapped
  consumer (`python3.11-env`, `symlinkJoin` outputs, etc.) still directly or
  transitively *references* the real, normally-named `openssl-3.2.1` path —
  references pass through wrappers fine, so a flat name search over the
  closure's path list won't miss the real dependency because of wrapping.
  The actual risk is the opposite: a flat name search can surface more than
  one *legitimately different* match (a build-time-only artifact, an
  unrelated environment that happens to share a name, a genuinely different
  version) with no principled way to auto-pick between them from the name
  string alone.

Both complications point the same direction: resolving a name to a single
store path is exactly the place ambiguity has to be surfaced loudly, not
guessed through.

### The refined design

Don't make "replace by name" an operation that decides what to touch on its
own. Once you have one exact `old` store path, `replace()`'s existing engine
already does the hard part correctly — it floods that identity through
every reference to it, however many times, wherever in the closure. The only
missing piece is a *safe way to find that one exact path* — so keep the
scope to exactly that:

- **`graft find <closure-root> <name>`** — a read-only search subcommand.
  Lists every distinct `(pname, version, output)` match in the closure, each
  with its full store path and enough context to tell them apart (at least
  one direct consumer, so "which occurrence is this" has an answer). Zero
  risk, since it only prints information.
- **`--override-name <name> <new>`** — sugar over `--override`, not a
  different mechanism. Resolves `name` against the closure; succeeds only
  if exactly one distinct match exists. If there's more than one, it refuses
  and prints the same candidate list `graft find` would, pointing the user
  at an exact `--override <old> <new>` instead of guessing.
- Matching should extract `pname` properly (strip the version suffix)
  rather than doing a raw substring match on the full `name-version`
  string — a substring match would both false-positive (`openssl` matching
  `libopenssl-thing`) and be brittle across version-string formats. Letting
  the user supply `pname-version` directly (e.g. `openssl-3.2.1`) should
  also work, to disambiguate multiple versions without a separate `find`
  step first.

This is a discovery/UX layer only — no change to the core replace engine,
and no new way to silently touch more than one distinct thing at once.
