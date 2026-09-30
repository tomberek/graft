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
- **`sed` matches the full basename; Nix's own scanner matches the bare
  hash alone — narrower than it needs to be.** Read directly out of
  `~/nix/src/libstore/references.cc`'s `search()` function (the real
  implementation the daemon's post-build reference scan uses): it walks the
  raw byte stream looking for any 32-character run that's valid
  nix32-alphabet (`BaseNix32::lookupReverse`) *and* present in the candidate
  hash set — with no requirement that a `-` or a name follows.
  `StorePath::HashLen = 32` confirms the window size matches this tool's own
  `HASH_LEN`. Two things this confirms, one
  reassuring and one not: the scan operates on the *printable base32 text*
  of the hash, not some other binary encoding, so `sed`'s literal-string
  substitution is matching the right representation and is naturally
  binary-safe (the scanner itself is a raw byte-level `std::string_view`
  walk with no line/NUL handling at all, carrying a `tail` buffer
  specifically so a hash split across two read chunks is still found — the
  exact same reason `graft_path`'s dump/sed/restore pipeline never had a
  binary-safety problem worth expecting). But `graft_path`'s `sed_expr`
  substitutes the full `hash-name-version` string
  (`store::basename`), not the bare hash — so a reference embedded as *just*
  the 32-character hash with no name suffix following it (which Nix's own
  scanner explicitly still counts, per the code above) would be missed by
  our rewrite and left as stale bytes in the grafted output. Not a
  regression versus precedent — nixpkgs's `replaceDirectDependencies` has
  the identical narrower match — but worth being precise about: we are not
  matching "the same hashes Nix itself would look for" in full generality,
  only the common case where the full basename string appears.
  `RewritingSink`'s own `assert(from.size() == to.size())` (used internally
  by Nix for self-reference masking) is independent confirmation that this
  tool's equal-length constraint is the same invariant Nix's own C++
  rewriting relies on, not something specific to this prototype.
- **Verified against real compiled binary content, not just text.** Every
  other fixture in `tests/fixtures/scenario.nix` is a shell script or plain
  text — `writeShellScriptBin`/`writeTextFile`/`runCommand` with no compiler
  involved — which never exercised the actual risk case above (embedded
  NUL bytes, an ELF `.dynamic` section, machine code around the reference
  string). `binOldLib`/`binNewLib`/`binConsumer` are real `cc`-compiled ELF
  objects: a shared library referenced through a genuine linker-emitted
  `RUNPATH` entry, confirmed via `readelf -d` to contain the literal store
  path. Grafting it and actually *running* the result (rather than just
  checking it exists) is what proves the rewrite is byte-correct against
  real binary content, not only against this project's own text fixtures.
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

## 8. Accepting installables, not just store paths

Every earlier example required both sides of `--replace` (and
`closure-root`) to already be built store paths — meaning a first-time user
had to run `nix build` twice themselves, by hand, before graft could do
anything. That's a real ergonomics gap: Guix users never see a hash, since
grafting is driven by a `replacement` field on a package, not a CLI argument.
`src/installable.rs`'s `resolve` closes most of that gap without touching
Guix's actual mechanism (a declarative field) — it just lets every
path-taking CLI argument (`closure-root`, both sides of `--replace`, `edit
drv`'s/`edit file`'s `path`) accept anything `nix build` itself accepts (a
flake reference, a `.drv` path) or a legacy `file.nix`/`file.nix#attr`
expression installable, building it via `derivation::nix_build` if it isn't
already realized.

One subtlety this had to get right: a store-path-*shaped* string (starts
with `/nix/store/`) is returned completely unvalidated — not canonicalized,
not existence-checked, not run through `nix build` — deferring entirely to
whatever the caller already does with it. Two reasons this matters, both
caught by the existing test suite rather than reasoned out in advance:

- **`replace`'s syntax-before-existence ordering.** `replace()` deliberately
  checks `--replace`'s basename-length constraint *before* requiring either
  side to exist (see §4/caveats — this is what lets it reject a bogus pair
  fast, without touching the store). `--replace old=old-longer-name` where
  `old-longer-name` was never built is exactly this test case: resolving it
  eagerly (via `nix build` or even just `canonicalize`) turns a clean
  "basenames differ in length" rejection into a confusing "don't know how to
  build this path" error, since the existence check now happens *before*
  `replace()` ever gets to run its own syntax check. Passing store-path-shaped
  strings through untouched preserves the original ordering exactly.
- **`edit drv`'s bare-`.drv` + `--output` disambiguation.** A bare `.drv`
  path for a multi-output derivation, combined with `--output <name>`, is
  meant to let `edit_drv::run` pick a specific output *after* the fact via
  `derivation::locate_output`. Running that `.drv` path through `nix build`
  during resolution would both build it prematurely and collapse the
  disambiguation this flag exists for (a bare `nix build <drv>` with no
  `^output` builds every output, not the one you asked for). Verified
  directly against the `multiOut` fixture: `edit drv <out> <drv> --output
  extra` still resolves the `extra` output specifically, not `out`.

Everything that isn't store-path-shaped goes through two branches: a
`#`-split first component that exists on disk and ends in `.nix` is treated
as a legacy `-f file.nix [attr]` installable (matching `edit_nix.rs`'s own
long-standing installable parsing, which `resolve` doesn't replace — `edit
nix` still has its own narrower parser, since it distinguishes "already
built" from "needs building" for a different reason); everything else is
handed to `nix build` as-is, covering flake references and anything else the
modern CLI resolves natively. `derivation::nix_build` — the actual `nix
build ... --no-link --print-out-paths` subprocess call, streaming stderr
live — is shared by `resolve`, `derivation::realise`, and `edit_nix::build`,
which previously each had their own near-identical copy of the same
spawn/capture/parse logic.

## 9. `graft nixos-system`: the other half of "patch what's running"

§8 removed the "pre-build both sides yourself" friction; the other big
ergonomics gap next to Guix is that grafting a live *system* still required
already knowing `/run/current-system` or `/nix/var/nix/profiles/system`
exists and typing it out. `src/nixos_system.rs` is a thin wrapper around
`replace`: same strategy flags, same engine, just defaulted to
`/nix/var/nix/profiles/system` — the same profile `nixos-rebuild` itself
operates on — instead of a required `closure-root` argument.

`--profile` is deliberately dual-purpose rather than two separate flags: it's
both *what closure to read* (resolved through `installable::resolve` exactly
like a `result`-style symlink into the store) and, if `--switch` is given,
*what `nix-env --set` registers the result into* afterward. Keeping it one
flag means it's always the same profile being read and written — pointing
`--profile` at a different generation, or a mounted image's system closure,
changes both consistently instead of risking a mismatch between "what I
read" and "what I updated."

`--switch test|switch|boot` (same action names as `nixos-rebuild`, so
there's nothing new to learn) is opt-in and does two things in sequence
after a successful graft: `nix-env --profile <profile> --set <new-root>`,
then `<new-root>/bin/switch-to-configuration <action>`. Without it, nothing
on the system changes — `run` just prints the new path plus the exact two
commands needed to apply it manually. This split (report vs. mutate) exists
because activating a system is a materially riskier operation than grafting
one (per this project's own risk-awareness policy, matching the reasoning in
§5/caveats around blind text substitution): a user should have to ask for it
explicitly, not get it as a side effect of asking "what would this graft."

Tested via fixture substitution rather than a real NixOS system (this
project's own dev environment isn't NixOS): `tests/fixtures/scenario.nix`'s
`systemLike` derivation exposes a stub `bin/switch-to-configuration` that
records how it was called, and the test points `--profile` at a plain
symlink in a tempdir rather than the real default — `installable::resolve`
treats a hand-made symlink and a `nix-env`-managed profile identically
(both are just a path resolving to a store path), so this exercises the
exact same code as the real default would, including confirming `nix-env
--set` actually repoints the profile symlink at the graft's result.

## 10. From one recursive walk to level-based parallel batches

The original `rewrite_one` was a single memoized recursive function: decide
what `path` resolves to, recursing into its own references first, grafting
or rebuilding inline, one path at a time. That's simple and was correct, but
it's also strictly sequential — every `nix build` call for every affected
path in the whole closure ran one after another, even when two paths had no
relationship to each other at all.

`replace()` now splits this into two passes:

1. **`classify`** — the same recursion `would_change`/`rewrite_one` used to
   do, but returning a `Node { category, level }` instead of just a bool or
   an already-rewritten path. `category` is exactly the five outcomes the
   old code implicitly had (`Explicit`, `Cutoff`, `Unchanged`, `NeedsGraft`,
   `NeedsRebuild`); `level` is new: `1 + max(level of every direct reference
   that itself needs a build)`, with `Explicit` references contributing `0`
   (they resolve immediately, no build to wait for) and `Cutoff`/`Unchanged`
   references not contributing at all. This is a completely ordinary
   longest-path-from-a-leaf leveling, which gives exactly the property that
   makes parallelism safe: **two nodes at the same level are provably
   independent** — if one were a transitive reference of the other, that
   would force its level strictly higher, by the `1 + max(...)` construction
   itself. No separate independence check is needed; it falls out of how
   levels are defined.
2. **Level-by-level, in three phases** — bucket every `NeedsGraft`/
   `NeedsRebuild` path by its level into a `BTreeMap<usize, Vec<PathBuf>>`
   (ordered, so levels process low to high), then per level:
   - **Construct** every path's recipe in parallel (`std::thread::scope`,
     one thread per path) — `graft_recipe`/`rebuild::rebuild_recipe`, which
     is everything `graft_path`/`rebuild_path` used to do *except* the
     final build: `nix derivation add` a synthetic or edited derivation and
     return its `.drv` path plus output name. Cheap (no building), but
     still worth spreading across threads since each one is a handful of
     subprocess round-trips (`which`, `nix path-info`, `nix derivation
     show`/`add`, retried on self-correction).
   - **Build** every recipe in the level with *one* `derivation::
     build_many` call — `nix build <drv1>^<out1> <drv2>^<out2> ...
     --no-link --json`, parsed by matching each result's `drvPath`/output
     name back to the recipe that produced it. This is the piece that
     actually bounds concurrency: a level with 200 independent paths used
     to mean 200 threads each spawning their *own* `nix build` subprocess,
     with nothing capping how many ran at once. One batched call instead
     hands the whole level to Nix's own daemon-side job scheduling — the
     same mechanism that safely builds all of nixpkgs, already respecting
     `--max-jobs`/`--cores` (forwarded via `nix_args` like every other
     build-related flag here) — rather than this tool reinventing a
     concurrency limit on top of subprocesses Nix would have queued anyway.
   - **Map back**: for each path, look up `(drv, output_name)` in the
     batch's result map and merge into `resolved`.

   All three phases read the *same* `resolved: HashMap<PathBuf, PathBuf>`
   built up so far, safely with zero locking: every reference a level-N
   path needs is guaranteed already resolved before level N starts (either
   non-building and seeded upfront, or itself at a strictly lower level,
   already merged), and nothing mutates `resolved` again until the whole
   level — construction and the batched build together — has finished. A
   hard barrier between levels, not a lock around shared state.

No new dependency: `std::thread::scope` (stable since Rust 1.63) is enough
for the parallel recipe-construction phase, since that "work" is spawning
and waiting on cheap subprocesses, not CPU-bound Rust code — and the actual
expensive concurrency (the builds themselves) is Nix's own job scheduler's
job, not this tool's, once recipes are batched into one call.

The same `classify` pass also answers "what happened, in aggregate" for
free — `Tally` counts every category across the whole closure and
`--dry-run`'s reporting loop was rewritten to read `Category` directly
instead of recomputing a verb from `would_change`'s boolean, so both the
prediction and the real run now share one source of truth for
classification. The closing summary line ("N grafted, M rebuilt, K cutoff, J
unchanged, I explicit replacement (T total)") is printed by `replace()`
itself, matching the existing precedent that dry-run's per-path reporting is
already direct `eprintln!` from inside the engine — real runs now follow
the identical pattern, including labeling each per-path line "grafted" or
"rebuilt" correctly (the old code's main.rs-side loop always said "grafted"
regardless of which strategy actually ran).

One correctness gap closed alongside this, not the point of the change:
`replace()` now warns explicitly when an `old` from `--replace` isn't in
`closure_root`'s closure at all, rather than silently doing nothing —
checked once, right after the closure is computed, against the full
`closure_paths` list.

## 11. `--out-link` and `--version`

Every build in this tool passes `--no-link` (see `derivation::nix_build`) —
deliberately, since the tool constructs and discards many intermediate
synthetic derivations per run and doesn't want a `result` symlink for each
one. The side effect: nothing produced here, including the *final* result,
was ever a GC root. A collection between a successful run and whatever the
caller does next could remove it. `--out-link <path>` (`derivation::
add_out_link`, `nix build <target> --out-link <path>`) roots the final
`new_root` on request — off by default (matching today's behavior exactly
when omitted), skipped under `--dry-run` (nothing new was built, so nothing
to root), and available on every subcommand via `StrategyArgs`.

`--version` was simply missing — `#[command(version)]` on the `Cli` struct
is all `clap`'s `Parser` derive needs to read it from `Cargo.toml`; it
doesn't do this automatically without being asked.

`--out-link` also writes `src/provenance.rs`'s one-line-per-run history to
`<path>.graft-history.jsonl` (timestamp, `std::env::args()` verbatim,
`new_root`) — since `--out-link` itself is necessarily a single symlink
pointing at only the *latest* generation, this is what makes "what did I
graft into this last week, and with what command" answerable after the
fact, rather than lost the moment a second run overwrites the symlink.
Deliberately just Unix-epoch integers, not a formatted date: pulling in a
date-formatting crate for one cosmetic improvement isn't worth it, and
`date -d @<timestamp>` converts it in one command if a human needs to read
it directly. Recorded as an appending JSONL file rather than a single
overwritten JSON object specifically so the *history* survives repeated
grafts into the same out-link, not just the most recent one.

## 12. `--report`: an HTML view of what a graft actually did

`--report <dir>` (works under `--dry-run` too) writes `<dir>/index.html`: a
dependency graph plus a details table, self-contained (no CDN, no build
step, no JS framework — just hand-written SVG and a `<style>` block).

The graph's layout isn't a new layout algorithm — it's `classify`'s
scheduling `level` (§10), reused directly for vertical position. That's not
a coincidence worth glossing over: "which nodes could build at the same
time" and "which nodes make sense to draw on the same row of a dependency
diagram" are the same question, so the same data answers both. Only
`Explicit`/`NeedsGraft`/`NeedsRebuild` nodes are included — `Cutoff`/
`Unchanged` are deliberately excluded, since a real closure's unchanged
majority is noise for a report meant to answer "what did this graft do,"
not a graph of the whole closure. Edges are drawn only between two *included*
nodes (a node's direct references that also made the cut), so the graph
shows the actual "story" subgraph, not a mess of everything each node
happens to depend on.

One real bug caught by actually looking at the rendered output, not just
checking the HTML was well-formed: the first version styled node labels as
white text (assuming they'd sit *inside* a colored circle) but positioned
them *below* the circle, on the page's light background — invisible.
Screenshotting the generated report (headless Chromium) during development
caught this immediately; reading the SVG source did not. Fixed by using a
dark label color, and separately, labels were switched from the full
`hash-name-version` basename to just `name-version` (`store::store_name`) —
the full hash prefix under every node made even a 5-node graph visually
noisy for no informational gain the details table (which does show full
paths) doesn't already provide better.

### `--report-diff`: embedding `nix-diff`/`diffoscope`

The base report shows *that* something changed and *what* it resolved to,
not *why*. `--report-diff` (meaningful only alongside `--report`, rejected
otherwise — `replace()` checks this before doing anything else) adds both:

- **`nix-diff`** (`src/diff.rs::nix_diff`, `nix-diff <old> <new> --color
  never`) only for `Explicit` and `NeedsRebuild` nodes — the concrete
  resolution to "what's the graft-mode diff story": grafting never touches
  the recipe, only the built bytes, so a `NeedsGraft` node's old and new
  derivers are identical and there's nothing for `nix-diff` to show.
  Confirmed empirically before writing the wrapper: `nix-diff` always exits
  `0` regardless of whether the derivations differ — unlike `diffoscope`
  below, its exit code carries no signal, so a nonzero exit here is a
  genuine invocation error, not "differences found." Embedded as a plain
  `<pre>` inside a collapsed `<details>` per row (its output is colored
  terminal text, not HTML; converting ANSI to HTML spans would be a real
  but separable nice-to-have over plain text).
- **`diffoscope`** (`src/diff.rs::diffoscope_html`) for any node with both
  an old and a *built* new path — not under `--dry-run`, where it's still
  `(pending)`. Uses diffoscope's own `--html <file>` mode directly (no HTML
  generation of our own needed) and links to it from the table.

Two real bugs surfaced only by actually running this against real
derivations, not by reasoning about the code:

- The report directory didn't exist yet when `diffoscope_html` tried to
  write into it — `report::write` only created it right before writing
  `index.html`, *after* the per-node diff pass that needs it already ran.
  Diffoscope doesn't create its own output directory, so this failed with a
  raw Python traceback, not a clean error. Fixed by creating the directory
  in `write_report_if_requested` before any diffing happens.
- `diffoscope`'s exit code alone can't distinguish "differences found"
  (exit `1`, intended) from "diffoscope crashed" (also exit `1`, confirmed
  empirically via the bug above's own traceback) — so `diffoscope_html`
  also checks that the output file actually exists before treating exit `1`
  as success. Exit code plus a real artifact, not exit code alone.

Every per-node diff failure is best-effort, not fatal to the report as a
whole (`replace.rs::diffs_for`): a missing tool, a path with no known
deriver, or a genuine diffoscope failure just omits that one node's diff
(logged under `-v`) rather than failing an otherwise-successful graft.
`pkgs.nix-diff`/`pkgs.diffoscope` are dev-shell-only in `flake.nix`
(diagnostic tooling, not core functionality — not in the packaged binary's
wrapped `PATH`), and tests run against the real tools rather than stubs,
matching this project's existing testing philosophy: every other test here
drives real `nix build`/`nix-store`, not fakes, and `nix-diff`/`diffoscope`
are no more special-cased than those.
