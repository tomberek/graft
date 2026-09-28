# Grafts: how Guix does it, what Nix already has, and this prototype

## 1. How GNU Guix's graft mechanism works

Guix packages can carry a `replacement` field:

```scheme
(define bash
  (package
    (name "bash")
    ;; ...
    (replacement bash-fixed)))
```

Whenever `guix build` / `guix package` / `guix system reconfigure` resolves a
package into a derivation, `guix/grafts.scm` checks whether anything in that
derivation's dependency graph has a `replacement`. If so, it doesn't rebuild
the dependent — it takes the dependent's *already-built* output, dumps it,
does a textual find-and-replace of the old dependency's store-file-name for
the new one, and restores the result as a new store item. This repeats
bottom-up through the whole dependency graph, so a single low-level
`replacement` (e.g. a patched `glibc` or `openssl`) propagates to every
installed package that depends on it — "on the order of a minute," per the
Guix manual, instead of a full-world rebuild.

Constraints the manual calls out explicitly:
- The **name and version string length of the replacement must equal** that
  of the original — the substitution is a fixed-width in-place rewrite of a
  string that's often embedded in binaries (RPATH, interned paths, etc.), so
  it can't grow or shrink.
- For shared libraries, the original and replacement **must have the same
  `SONAME`** and be ABI-compatible, or dynamic linking breaks at runtime.
- Grafting is on by default; `--no-grafts` opts out for a from-scratch
  rebuild instead.

## 2. Nix already has the same primitive — it's just not systemic

nixpkgs ships the identical low-level trick, and has for years:

- **`pkgs/build-support/replace-direct-dependencies.nix`** is the actual
  graft: `nix-store --dump $drv | sed -f rewrite.sed | nix-store --restore
  $out`, where `rewrite.sed` maps old-dependency-basename →
  new-dependency-basename. Same same-length constraint, checked up front
  (`cannot rewrite $oldBasename to $newBasename: length does not match`).
- **`replace-dependencies.nix`** (`replaceDependencies` / `replaceDependency`)
  builds a full transitive rewrite on top of that: it computes the whole
  reference graph via `exportReferencesGraph`, memoizes a lazy Nix attrset
  mapping every store path in the closure to its rewritten form, and applies
  the direct-replace primitive bottom-up. There's real test coverage for
  this in `nixos/tests/replace-dependencies/`.

So the technique isn't missing from the Nix ecosystem. What's missing is
everything that makes Guix's version feel like a platform feature rather
than a party trick:

| | Guix | nixpkgs `replaceDependencies` |
|---|---|---|
| Trigger | Automatic, on every build/install, for any `replacement` field | Opt-in — you must call the function yourself |
| Scope | Whole package graph, driven by the daemon | Whatever closure you hand it, computed via Nix-language eval |
| Cost to compute the graph | Native, daemon-side | `exportReferencesGraph`, which is a **build** — i.e. import-from-derivation, needs `allow-import-from-derivation`, and is slow |
| Distro integration | CVE tracker ↔ `replacement` ↔ `guix system reconfigure` | None — no nixpkgs-wide security-patch pipeline consumes this |
| Applies to | nixpkgs packages | nixpkgs packages (needs a Nix expression; can't point it at an arbitrary already-built store path like a running system profile without writing one) |

## 3. The road Nix actually went down instead: content-addressed derivations

Nix's own answer to "avoid rebuilding the world for a leaf security fix" has
mostly been **content-addressed derivations** (RFC 62), not grafts: a CA
derivation's output path is a hash of its *content*, not its *inputs*. If you
rebuild a low-level dependency and rebuild everything one level up, but a
level-up package's output happens to be byte-identical to before, its output
path doesn't change — so *its* dependents don't need to rebuild ("early
cutoff"). This is a different, more conservative tradeoff than grafting:

- CA derivations still **rebuild every directly-affected package once** —
  they only cut off the cascade *above* that. Grafts rebuild nothing at all.
- CA derivations don't have the same-length-name constraint or the binary-
  corruption risk of blind text substitution — the output is a real,
  correct rebuild, just deduplicated by content.
- The two are complementary, not competing: you could imagine CA derivations
  handling "rebuild this and stop the cascade if nothing downstream actually
  changed," and grafts handling "I don't even want to pay for the one
  rebuild, patch it in place."

## 4. This prototype: `graft`

The concrete gap this fills: nixpkgs's mechanism only operates *inside* a Nix
expression, over a closure it computes itself via a build. This tool
operates directly on the Nix store/daemon (`nix build`, `nix store`, `nix
path-info`, `nix derivation`, and — in one deliberate, documented spot — the
legacy `nix-store`) so it can graft *any* already-realized closure —
including one you don't have (or want to re-derive) a Nix expression for,
like a live system profile — without triggering IFD.

### Core command: `replace`

```
graft replace <closure-root> --replace <old>=<new> [--replace <old>=<new> ...] [--dry-run] [-- <extra nix build args>]
```

Anything after a literal `--` is forwarded verbatim to every `nix build`
call this makes to realise a graft (same for `edit drv`'s and `edit nix`'s
own build calls). This isn't speculative: while testing this locally, the
build for a graft derivation stalled for a full minute because this
environment has a remote build machine configured
(`ssh://builder@remote-builder.example.com`) that's unreachable here,
and Nix tries it before falling back to local. `graft replace ... --
--builders ''` (or `-v`/`-Lv` to see what's actually happening instead of
guessing) fixes that per-invocation, without needing to edit `nix.conf`.
Confirmed the stall was real graft work, not an accidental full rebuild, by
inspecting the stuck derivation's JSON mid-hang: its `args` were exactly the
tiny dump/sed/restore recipe below, not consumer's actual build recipe.

This tool originally drove all of this through legacy `nix-store`, then
moved everything with a clean modern equivalent over to the unified `nix`
CLI (`nix path-info`, `nix store add`/`dump-path`, `nix build`) — mainly
because that stall was exactly this: legacy `nix-store --realise` not
reliably taking build-related flags the way every `nix` subcommand does.
Confirmed each swap byte-for-byte/behaviorally equivalent before switching
(same output for `nix-store -qR` vs. `nix path-info -r`, `--dump` vs. `store
dump-path`, `--add-fixed sha256 --recursive` vs. `store add --mode nar
--hash-algo sha256`, `-q --deriver` vs. `path-info --derivation`). The one
holdout is `nix-store --restore` (unpacking a NAR to an arbitrary
non-store directory): there's no modern `nix store` equivalent for that, and
since it's a pure local filesystem operation with no build/substituter
behavior, there's no flag-passthrough concern to fix there anyway.

"Replace all of `old` with `new`, in the context of the closure rooted at
`closure-root`." Concretely:

1. Reject any `--replace old=new` pair up front if `basename(old)` and
   `basename(new)` differ in length — same reasoning as Guix/nixpkgs: we're
   about to do a fixed-width byte substitution inside a NAR stream that other
   store items may embed as fixed-length strings.
2. `nix path-info -r <closure-root>` gives the full closure `C`.
3. For every path in `C`, `nix path-info --json <path>` gives direct
   deps (its `references` array — there's no modern subcommand that lists
   just the direct references the way legacy `nix-store -q --references`
   did). A memoized post-order walk (leaves first) decides, for each path,
   whether any direct reference changed:
   - explicit `old` keys map straight to `new` (trusted as-is, not recursed
     into — matches Guix/nixpkgs semantics);
   - a path with no changed references maps to itself, no new store item;
   - a path with changed references gets **grafted** (see below).
4. Return the (possibly-rewritten) path for `closure-root`.

**Grafting a single path** turned out to need a real derivation build, not
the simpler thing tried first. The initial design added the rewritten NAR
straight into the store via `nix-store --add-fixed sha256 --recursive`
(dump → substitute basenames → restore to a scratch dir → content-address
it) — no sandboxed build round-trip, and grafting the same bytes twice
verifiably produces the same store path (content dedup, confirmed by
`--add-fixed`ing the same tree from two different scratch locations and
getting identical hashes). It seemed strictly simpler than constructing a
synthetic `.drv`.

It's wrong, though: **`nix-store --add-fixed` never registers references.**
Confirmed by grafting a fixture this way and running `nix-store -q
--references` on the result — empty, despite the new dependency's basename
being clearly present in the output's content. That's a real correctness
gap, not a cosmetic one: an unregistered reference means the thing it points
at can be garbage-collected out from under the grafted path (silent
corruption), and it means this tool's own closure walk, which relies on
`references` (via `nix path-info --json`), would go blind past that node for
any *further* graft applied above it. The obvious fix, `nix-store --register-validity`
(explicitly telling the store DB what a path's references are, no scanning
required) — turns out to need trusted-user/root privileges, which would make
this tool unusable for an ordinary developer.

So `graft_path` instead builds a tiny **synthetic derivation** whose builder
does exactly the dump/substitute/restore, and realises it through the normal
build path — the same structural choice Guix and nixpkgs's
`replaceDependencies` both make, confirmed correct by running nixpkgs's own
`replaceDependency` on the same fixture and inspecting its result: the
grafted output correctly referenced *both* the new dependency *and* an
unrelated, unchanged dependency (`bash`, from the shebang line) that was
never explicitly declared anywhere in `replaceDependency`'s own inputs. That
told us two things: the daemon's post-build reference scan is what actually
registers references (no privileged step required from the caller), and to
be safe this tool declares every direct reference of the path being grafted
— changed *or* unchanged — as an inputSrc of the synthetic derivation, so the
scan has a chance to see each one regardless of exactly how its candidate set
is scoped.

Concretely, for a path with changed references `(old₁,new₁), (old₂,new₂), …`:
build a derivation whose builder is a store-provided `bash` running
`nix-store --dump <path> | sed 's|old₁|new₁|g;s|old₂|new₂|g;…' | nix-store
--restore "$out"` (still legacy `nix-store` *inside the sandboxed builder
script* — that's a fixed recipe with no caller-supplied flags to pass
through, so there's nothing the modern CLI buys it here), with `inputSrcs` =
`{path} ∪ {new₁, new₂, …} ∪ {every other direct reference of path, changed or
not} ∪ {the store items providing bash/sed/nix-store}`. Realise it (`nix
build <drv>^out`, forwarding any `nix_args`) and that's the grafted path.

One more real wrinkle surfaced by this: `nix derivation add` requires the
`.drv`'s output path (and any `env` var that mirrors it) to already match
Nix's own computed hash for that derivation — a hash with no public API
outside Nix's C++ implementation. Rather than reimplement
`hashDerivationModulo`, this tool submits a syntactically-valid placeholder
output path, reads the *actual* value back out of `nix derivation add`'s own
error message (`derivation has incorrect output '...', should be '...'`),
patches it in, and retries — safe only because Nix itself validates the
correction each time. The exact same trick is reused by `edit drv` (see
below), which has the identical problem for a different reason (there, the
user edited the derivation's `env`/`args` directly).

Reusing the original item's name-version suffix for a graft's scratch
directory (and thus its resulting store item) means every graft this tool
performs automatically satisfies the same-length constraint for *its own*
basename when something further up the closure needs to reference it in
turn — only the user-supplied top-level `--replace` pairs need the explicit
check in step 1.

### `--rebuild`: the other mode

Guix's manual is explicit that grafting is a default you can turn off
(`--no-grafts`) to get a real rebuild instead — the two modes solve the same
problem with different tradeoffs, and it's worth having both. `graft
replace ... --rebuild` mirrors that: instead of `graft_path`'s blind NAR
byte-substitution, it finds the affected path's actual deriver
(`nix path-info --derivation`), edits that derivation's JSON *structurally* —
swap the entry in `inputs.drvs` (if the old dependency has its own deriver)
or `inputs.srcs` (if it doesn't), plus any `env`/`args` string that mentions
the old path or its basename — and does a real, sandboxed rebuild of the
original recipe via the same `add_with_retry`/`realise` primitives `replace`
already uses.

Two real, verified differences from graft mode:
- **No equal-length-basename constraint.** Confirmed with a fixture pair
  named `dep` (37-char basename) and `dependency-with-a-much-longer-name`
  (68-char basename), both exposing the same `bin/dep` entry point via
  `writeTextFile`'s `destination` (decoupled from the Nix `name` that
  controls the store path): graft mode correctly refuses this pair, and
  `--rebuild` succeeds. Structural JSON edits don't care about byte lengths
  the way an in-place NAR rewrite does.
- **Requires a known deriver.** A path with no deriver (content directly
  `--add`ed to the store, or a previous graft) has no recipe to rebuild —
  `--rebuild` bails with a message suggesting graft mode for that path
  instead. Graft mode has no such requirement, which is part of its appeal:
  it works on paths with no build recipe at all.

`edit file`/`edit drv`/`edit nix` each also take `--rebuild`, controlling
only the propagation *above* their own edit (their own leaf action — the
file edit, or the initial rebuild in `edit drv`/`edit nix` — is unaffected).

### `--cutoff`/`--force-rebuild`: per-path strategy, not just a global switch

`--rebuild` is a global default for the whole operation. Real closures need
finer control than that: grafting is good for most of a closure, but there
are specific places in the graph where it's actively wrong to graft, and
other places where a rebuild is wanted without paying for it everywhere.
The concrete example that motivated this: a NixOS image or system closure
embeds a store database/registration dump — a text file listing every store
path in the closure along with its *separately recorded* NAR hash. Grafting
only rewrites store-path-reference strings; it has no idea that some other
byte range nearby is a hash of content that a graft further down the graph
just changed. Graft that registration-bearing path and the path references
inside it get correctly updated, but the recorded hashes next to them go
stale — internally inconsistent output that looks fine until something
reads that registration data expecting it to be true.

So `replace` (and by extension every `edit_*` subcommand, which all funnel
into it) takes two more path lists, mirroring nixpkgs's
`replaceDependencies`'s `cutoffPackages` option:

- **`--cutoff <path>`** (repeatable): never touch this path, no matter what
  changed beneath it — not even a graft. `rewrite_one` returns it unchanged
  without even checking its own references, so propagation genuinely stops
  there; anything above it that only depends on it through this path sees
  "nothing changed" too. This is the safe answer for the registration-dump
  case above: don't let the graft mechanism anywhere near it.
- **`--force-rebuild <path>`** (repeatable): if this path changes, always
  use the rebuild strategy for it specifically, regardless of the global
  `--rebuild` flag. For a node that isn't unsafe to graft, but where a real
  rebuild is wanted anyway (e.g. it's cheap enough, or its own downstream
  consumers care about provenance).

Both were verified end-to-end on a 3-level `leaf -> mid -> top` chain: with
`--cutoff mid`, replacing `leaf` produced `top` completely unchanged (same
store path as before — the cutoff at `mid` genuinely stopped propagation
before it reached `top`, not just "reported as clean"). With `--force-rebuild
mid` (global mode left at the graft default), the resulting `mid`'s deriver
runs the real `stdenv`/`default-builder.sh` builder while `top`'s deriver —
one level up, using the default strategy — still runs the
`nix-store --dump | sed | nix-store --restore` recipe. Mixed strategies in
the same walk, confirmed by inspecting both derivations' actual builder args,
not just trusting the log output.

Precedence when a path matches more than one control: an explicit
`--replace old=new` target wins over `--cutoff`, which wins over the default
strategy (`--rebuild` or plain grafting) — the same order nixpkgs documents
for `cutoffPackages` vs. `replacements` in `replaceDependencies`.

Implementation note: `rewrite_one`/`would_change` took on enough
strategy-related parameters (`nix_args`, `full_rebuild`, `cutoffs`,
`force_rebuild`, plus `explicit`) that clippy's `too_many_arguments` flagged
both `replace()`'s signature and every `edit_*::run()` — the actual trigger
for introducing `ReplaceOptions`, a small struct bundling everything about
*how* to replace, separate from *what* to replace. `replace()` and every
`edit_*::run()` now take it as one argument instead of five-to-seven.

### Editing capability: producing `(old, new)` from a small change

Rather than requiring a pre-built replacement, three subcommands let you make
a small edit and have the tool derive `(old, new)` itself, then hand it to
the exact same `replace` engine (walking the rewrite up through `C`). Each
also takes a trailing `-- <extra nix args>`, forwarded to its own build step
(`edit drv`'s and `edit nix`'s initial rebuild; `edit file` has none of its
own) and to every graft built on top of it via `replace`.

**`graft edit file <closure-root> <path> [<subpath>]`** — dump `<path>`,
restore to a scratch directory, open `$EDITOR` on `<subpath>` (or the whole
extracted tree), re-add via `--add-fixed --recursive` as `new`. No
same-length constraint here: this is an ordinary file edit inside a NAR,
which encodes explicit lengths per entry — unlike the pure reference-swap
case, content can grow or shrink freely. This subcommand *does* still use
the simpler, ultimately-rejected `--add-fixed` approach from the `replace`
design discussion above, and inherits its reference-registration gap: if
your edit introduces or preserves a reference to another store path, that
reference won't be registered. Fixing that properly would mean generating a
sandboxed-build recipe for an arbitrary user edit, which is a meaningfully
bigger problem than the fixed dump/sed/restore recipe `replace` needed — left
as a known limitation rather than solved here (see caveats below).

**`graft edit drv <closure-root> <drv-path>`** — `nix derivation show
<drv-path>` returns `{"derivations": {"<basename>.drv": {...ATerm-JSON...}}}`;
the inner object (minus the `derivations` wrapper) is exactly what `nix
derivation add` accepts back — confirmed by round-tripping an unmodified
derivation and getting the identical `.drv` path back. After a `$EDITOR`
session on that inner JSON, adding it back will almost never succeed on the
first try: Nix validates that `outputs.<name>.path` (and any `env` var that
mirrors it, e.g. `env.out`) matches its own computed hash, and *tells you the
correct value* in the error (`derivation has incorrect output '...', should
be '...'`) rather than computing it for you. There's no public API for this
hash (`hashDerivationModulo`) outside Nix's own C++ implementation, so this
tool does the pragmatic thing: patch the JSON fields the error names, retry,
repeat until `nix derivation add` succeeds or an unrecognized error appears.
This is a real hack, called out here and in the code — it's safe only because
Nix itself is the one validating our guess each time. Once added, `nix
build <drv>^out` actually builds the edited derivation (there's no way
around one real, sandboxed rebuild here — the recipe changed, not just a
reference) and its output feeds `replace` as `(old, new)`.

**`graft edit nix <closure-root> <installable>`** — the one place this
tool does use Nix-language evaluation, deliberately scoped to a single
attribute rather than nixpkgs's whole-closure IFD walk. Best-effort: records
the installable's current output path (if it has one), opens `$EDITOR` on
the backing `.nix` file (for `file.nix#attr` this opens `file.nix` as a
whole — there's no attempt to seek to `attr`'s exact location), then runs
`nix build <installable> --no-link --print-out-paths` to get the new output.
Feeds the same `(old, new)` into `replace`.

## 5. Known caveats

- **Blind text substitution can corrupt unrelated data.** If a store path's
  basename happens to appear as a substring inside binary data that *isn't*
  actually a store reference, this tool (like Guix, like nixpkgs) will
  rewrite it anyway. Store path basenames are high-entropy 32-character
  hashes, so collisions are astronomically unlikely, but this is a
  fundamentally unverified rewrite, not a semantically-aware one.
- **No SONAME/ABI check.** Guix's manual explicitly warns that grafting a
  shared library requires matching `SONAME` and binary compatibility; this
  tool doesn't check either — that's on the caller.
- **`--rebuild` can still fail to find a substitution point, though now it
  says so.** It only recognizes a dependency as a whole `inputs.drvs`/
  `inputs.srcs` entry (matched by output name, see §6) or a literal
  substring of an `env`/`args` string. A dependency referenced some other
  way — most concretely, embedded only *transitively* through another
  declared input's own build output — won't be found, and `rebuild_path`
  now errors instead of silently rebuilding an unmodified derivation (see
  §6). Multi-output derivations are fully supported as of §6, which used to
  be a separate, harder limitation here.
- **`nix store add` never registers references** (see §4 — discovered via
  its legacy predecessor `nix-store --add-fixed`, and confirmed the modern
  command has the identical behavior) — a real bug this tool has to route
  around for `replace`/`edit drv` (via a synthetic derivation build) but
  still carries for `edit file`, which has no equivalent workaround yet. A
  file edit that references another store path will silently produce a
  store item with no recorded reference to it.
- **No substituter trust story.** Every grafted or rebuilt path here was
  produced locally; no binary cache has ever seen it, so there's nothing that
  will substitute it elsewhere, and no mechanism here to sign or publish one.
- **O(n) `nix path-info` subprocess calls**, one per closure member for
  its `references`, rather than nixpkgs's single bulk
  `exportReferencesGraph`-style query. Fine for a prototype; a real
  implementation would batch this (e.g. via the Nix daemon's worker protocol
  directly, or `nix-store --query --graph`).
- **The `nix derivation add` output-path retry loop (§4) is inherently
  fragile** — it depends on the exact wording of two specific Nix error
  messages, shared by `replace`'s synthetic-derivation construction and by
  `edit drv`. If a future Nix version changes that wording, the loop just
  fails closed (an unrecognized error aborts it) rather than silently doing
  the wrong thing, but it's worth calling out as the shakiest part of this
  prototype.
- **`replace`'s synthetic derivation assumes a store-provided `bash`/`sed`
  are on `$PATH`** (see `flake.nix`'s devShell) and that they, plus
  `nix-store`, resolve to paths actually inside `/nix/store` — a bare-metal
  install where these are system binaries (e.g. `/usr/bin/bash`) won't work,
  since only store paths can be declared as sandboxed-build inputs. The
  packaged binary (`nix build`) only wraps `PATH` with `nix`'s own `bin`
  (needed for `nix derivation add`/`nix eval`/etc.) — it does *not* fix this
  for `bash`/`sed`, so running the built package outside a shell that already
  provides store-path `bash`/`sed` will hit the same `tool_path` error.
- **`tests/integration.rs` cannot run inside `nix build`'s `checkPhase`**
  (confirmed: all four tests failed there with `nix-build: No such file or
  directory`) — they drive real `nix-build`/`nix-store`/`nix derivation`
  commands against a live daemon and store, which a sandboxed package build
  sandbox denies by design (no daemon socket, no unrelated binaries on
  `$PATH`). `flake.nix` sets `doCheck = false` for that reason; the tests are
  meant to run via `nix develop -c cargo test`, on a real machine, not as a
  build-time check.

## 6. Two graphs, not one: what Guix's "Grafts, continued" got right, and where `--rebuild` initially got it wrong

Guix's own [2020 writeup of grafts](https://guix.gnu.org/blog/2020/grafts-continued/)
states the core design rule plainly: whether a package needs grafting is
decided from its *actual runtime references* — what the build daemon's
post-build scan finds embedded in the output — never from declared
build-time inputs. `native-inputs` is explicitly called "just a hint": "we
first have to actually build `coreutils` before we can tell whether it
depends on `perl` at run time." `guix build coreutils` and `guix build
coreutils --no-grafts` return the same thing precisely because coreutils
doesn't *reference* perl at runtime, even though perl is a build input. (The
1.1.0 rework the post actually describes is a batching fix — using delimited
continuations to intercept `build-derivations` so substitute-fetching and
grafting don't serialize package-by-package — not a change to *which* graph
the decision is based on.)

graft's closure walk (`replace.rs`'s `rewrite_one`/`would_change`, both
recursing via `store::references()` → `nix path-info --json`'s `references`
field) already matched this: it decides whether a node needs touching purely
from the runtime reference graph, never from `.drv` structure. That part was
right from the start. But once a node *is* decided to need touching,
`--rebuild` has to find *where* inside that node's `.drv` to apply the
change — and that search operates on a genuinely different graph: the
`.drv`-to-`.drv` input graph (`inputs.drvs`, `inputs.srcs`), one hop, as
declared before the build ever ran. These two graphs aren't always the same
shape. Concretely reproduced in `tests/fixtures/scenario.nix`
(`transitiveLeaf`/`transitiveWrapper`/`transitiveTop`): `transitiveTop`
directly depends only on `transitiveWrapper` — that's the only thing its own
`.drv` declares — but `transitiveWrapper`'s content embeds
`transitiveLeaf`'s store path as plain text, and `transitiveTop`'s builder
copies that content verbatim into its own output. Nix's post-build scan
finds `transitiveLeaf` in there (it's reachable in the sandbox transitively,
through the wrapper's own closure) and correctly registers it as a genuine
runtime reference of `transitiveTop` — confirmed directly via `nix path-info
--json`. But `transitiveTop`'s own `.drv` never mentions `transitiveLeaf` at
all: not in `inputs.drvs`, not in `inputs.srcs`, not as a literal substring
of any `env`/`args` string. Before this fix, `rebuild_path` would proceed
anyway: the derivation JSON came back byte-identical to the original, so
`nix derivation add` returned the same `.drv`, `realise` returned the
*original* output path, and the caller was told a rebuild happened when
nothing actually changed. `substitute_dependency` now returns whether it
found *anything* to change, and `rebuild_path` treats "found nothing" as a
hard failure, naming the path and the unlocatable dependency and suggesting
`--cutoff` or falling back to the default graft strategy — which operates on
realized bytes, not declared structure, so it doesn't have this gap at all
(confirmed: the identical replacement succeeds cleanly under graft mode on
the same fixture).

Worth noting why neither Guix's own grafts nor nixpkgs's
[`replaceDependencies`](https://github.com/NixOS/nixpkgs/blob/c89322c9af308a079f0ff5167fc785f1a48cf6d8/pkgs/build-support/replace-dependencies.nix)
ever hit this: neither has a structural-rebuild mode at all.
`replaceDependencies` always bottoms out in `replaceDirectDependencies` — the
same blind `nix-store --dump | sed | nix-store --restore` recipe this tool's
own `graft_path` uses. `--rebuild` is a genuine addition beyond both
reference implementations, and this whole class of bug is specific to having
added it — the tradeoff of getting a real rebuild instead of a byte patch is
exactly this: a second, different graph you now have to reconcile against
the first one. (Separately, nixpkgs's own file documents a related but
distinct simplification it deliberately doesn't handle: "another replacement
could introduce the dependency [into relevance]... handling this corner case
would add significant complexity... we just leave it to the user." Same
philosophy — name the edge case, don't silently get it wrong, don't over-engineer
the fix — applied to a different edge case than the one above.)

**Multi-output derivations** surfaced from the same re-read.
`derivation::single_output` used to hard-reject any derivation with more
than one output, meaning `--rebuild` could never touch a multi-output node
at all (grafting, being purely byte-level, never had this problem — this was
a `--rebuild`-only gap). Replaced with `derivation::locate_output`, which
finds *which* output name a given output path corresponds to regardless of
how many *other* outputs the same derivation has. `substitute_dependency`
now resolves both the old and new dependency's own output names too, and
edits the specific per-drv `outputs: [...]` name list in `inputs.drvs`
(dropping just the old output's name, adding just the new one — not moving
the whole entry wholesale, which would have been wrong for a
multiple-output-of-the-same-drv dependency). Verified end-to-end in
`tests/fixtures/scenario.nix` (`multiOut`, two outputs `out`/`extra`;
`multiConsumer` depends specifically on `extra`): the rebuild correctly
tracked `inputs.drvs.<multi.drv>.extra` while leaving `out` alone, and the
`nix derivation add` self-correction retry loop (built for the single-output
case originally) turned out to generalize for free — it corrected both
`env.out` and `env.extra` without any multi-output-specific code, since
"incorrect environment variable" is handled generically regardless of which
variable name Nix names in the error.

**`--interactive`/`-i`**: the user's framing for this whole feature area —
"like an interactive git rebase: a change in the build graph can have
different strategies on the way up to the toplevel" — is literally what
`--cutoff`/`--force-rebuild`/`--force-graft` already let you specify
non-interactively (pick a strategy per node upfront). `--interactive` is the
same mechanism with a UX layer on top, modeled directly on `git rebase -i`'s
todo list: it runs the exact same `would_change` discovery walk `--dry-run`
uses to find every affected path, writes one line per path as `<strategy>
<path>` (defaulting to whatever the already-configured flags say) with a
git-rebase-todo-style instructional comment block, opens `$EDITOR` on it
(reusing `editor::edit`, already shared by `edit file`/`edit drv`), and
parses the result back into the same `cutoffs`/`force_rebuild`/`force_graft`
sets — no new engine, since the walk itself never changes. Deliberately
narrower than git's version: no reordering (dependency order is
graph-determined, not a free choice) and no deletion-means-drop convention —
every line from the original list must still be present when saved, just
with a possibly different strategy word, or the whole operation is rejected
with a specific "must not be added"/"must not be deleted" error rather than
silently doing something the user didn't ask for.

## 7. Detecting rebuild infeasibility before attempting anything

§6 made `--rebuild` fail loudly, at rebuild time, when the `.drv`-structural
graph doesn't contain a reference the runtime graph says needs changing. That
was the right first response, but it means the failure only surfaces after
`--dry-run`/`--interactive` have already told the user a rebuild would
happen, and (outside of dry-run) only after the real `nix derivation add` +
build machinery has already run for every other node in the closure. Better
to know upfront, during discovery, so `--dry-run` reports the truth and
`--interactive`'s todo list never proposes a strategy that's certain to
fail.

`rebuild::unlocatable_dependencies(path, changed_old_refs)` answers exactly
this, without building or mutating anything: `Ok(None)` if `path` has no
deriver at all (nothing to rebuild, regardless of which dependency);
otherwise `Ok(Some(missing))` listing exactly which of `changed_old_refs`
`substitute_dependency` would fail to find. It's a read-only probe built by
reusing `substitute_dependency` itself — called with `old` passed as *both*
the old and new value, a no-op substitution (every branch matches `old`
against itself and puts it right back) — rather than a second "would this
match" implementation that could silently drift out of sync with what a real
rebuild attempt actually does.

`replace.rs` calls this in two places:

- **`--dry-run`**: for every path whose strategy would be `rebuild`,
  `report_rebuild_feasibility` checks it and prints either "would rebuild
  `<path>`" or a specific "would attempt `--rebuild` for `<path>` but
  `<deps>` [is/are] not declared in its own `.drv` ... this WILL fail" —
  naming exactly which dependency is the problem and suggesting
  `--force-graft`/`--cutoff` — instead of the previous generic "would
  rebuild" that turned out to be wrong once the build was actually attempted.
- **`--interactive`**: `interactive_select` precomputes infeasibility for
  every affected path before writing the todo file. An infeasible path's
  line defaults to `graft` (not whatever the global `--rebuild`/
  `--force-rebuild` setting would otherwise pick) with an inline `# rebuild
  not possible: <deps> not declared in its own .drv` comment explaining why,
  and explicitly selecting `rebuild`/`r` for that line is rejected when the
  file is parsed back, with the same explanation, rather than being accepted
  and failing later mid-walk.

Same fix applied in both places: explicit `--replace` targets (the path(s)
named directly on the command line, as opposed to paths pulled in
transitively by the closure walk) are no longer run through the
graft-vs-rebuild feasibility machinery at all — they're always rewritten via
whatever mechanism `replace()` core already uses for the literal
replacement, so labeling them "would rebuild"/"would graft" was misleading.
`--dry-run` now prints "is an explicit replacement target -> `<new>`" for
these, and `--interactive` excludes them from the affected list entirely,
since picking a strategy for them has no effect.

Deliberately not implemented: automatically falling back from `rebuild` to
`graft` when a path is detected as infeasible. The exact same structural
symptom — "rebuild can't locate this dependency in the `.drv`" — can mean
either "safe to graft instead" (`transitiveTop`, where the byte-level
substitution is exactly as correct as a real rebuild would have been) or
"unsafe to graft" (a node whose whole reason for a `--cutoff`/
`--force-rebuild` override was that grafting it would corrupt something a
byte patch can't safely touch, e.g. anything resembling a store database or
registration file). The tool cannot tell these apart from the symptom alone,
so any such fallback has to stay an explicit, loudly-logged choice the user
makes (`--force-graft`, or picking `graft`/`g` in `--interactive`) — never
automatic.
