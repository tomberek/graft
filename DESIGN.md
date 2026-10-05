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
graft replace <closure-root> --override <old> <new> [--override <old> <new> ...] [--dry-run] [-- <extra nix build args>]
```

Anything after a literal `--` is forwarded verbatim to every `nix build`
call this makes to realise a graft (same for `--edit`'s drv-edit/nix-edit
mechanisms' own build calls — see §14/§15). This isn't speculative: while testing this locally, the
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

1. Reject any `--override old new` pair up front if `basename(old)` and
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

**Matching the same width Nix's own scanner does.** Read directly out of
`~/nix/src/libstore/references.cc`'s `search()` function (the real
implementation the daemon's post-build reference scan uses): it walks the
raw byte stream looking for any 32-character run that's valid
nix32-alphabet (`BaseNix32::lookupReverse`) *and* present in the candidate
hash set — with no requirement that a `-` or a name follows.
`StorePath::HashLen = 32` confirms the window size matches this tool's own
`HASH_LEN`. The sed expression above originally substituted only the full
`hash-name-version` string (`store::basename`), narrower than that: a
reference embedded as *just* the 32-character hash with no name suffix
following it (which Nix's own scanner explicitly still counts) would be
missed and left as stale bytes in the grafted output — not a regression
versus precedent (nixpkgs's `replaceDirectDependencies` has the identical
narrower match), but still a real gap. Closed by appending one more `s///`
per pair, `store::store_hash`-to-`store::store_hash` (just the 32-char
prefix, not the full basename), applied *after* the full-basename rule in
the same sed invocation — order matters here only in the sense that the
full-basename rule already consumes the common case first, so the
bare-hash rule only ever catches what that one's own replacements left
behind, never double-substituting. The self-reference rule (above) gained
the identical second clause for the same reason, computed from
`${new_self:0:32}` rather than an external tool. Verified with a fixture
that embeds just a bare hash, confirmed via a direct `nix-store -q
--references` check that Nix's own scanner already registers it, then
grafting and checking the hash was rewritten *and* the reference stayed
registered afterward (`replace_rewrites_a_bare_hash_reference_with_no_name_suffix`).

**Self-references.** A path can embed its own basename somewhere in its
content (confirmed via Nix's own `-q --references`: a path that does this
genuinely references itself, and the daemon's post-build scan records it)
— independent of whatever dependency actually triggered the graft. Naively,
this can't be rewritten the same way as a real `(old, new)` pair: the new
self-basename *is* this derivation's own output path, which Nix hasn't
assigned yet while this script is still being constructed in Rust — using
it here looks circular. It isn't, though, for an *ordinary* (non-fixed-
output) derivation like this one: Nix computes the output path from the
derivation's declared structure alone (builder, args, env, inputs), never
from what the builder actually produces, and hands that already-decided
value to the builder as `$out` *before* the builder runs. So the fix needs
no coordination with the Rust side at all — a second `sed` pass, appended
to the pipeline above, computed *inside the sandbox* after `$out` is
known: `sed "s|<old-self-basename>|${out##*/}|g;s|<old-self-hash>|${new_self:0:32}|g"`
(bash's own basename/substring via parameter expansion, not an external
binary, so no extra sandboxed input to declare for it; the second clause
is the same bare-hash widening the dependency rules above also needed). A
no-op when `path` has no self-reference, the common case. Verified by
grafting a fixture that embeds `$out` in its own content
independent of the dependency actually being replaced
(`replace_rewrites_a_self_reference_to_the_grafted_result_s_own_path`):
before this, the embedded text kept naming the *pre-graft* hash and the
grafted output's own `references` ended up including that stale pre-graft
path instead of itself (never garbage-collectable out from under it, and
silently wrong if the self-reference was functionally load-bearing); after,
both the dependency and the self-reference resolve correctly, and `-q
--references` shows a genuine self-reference to the new path, not the old
one. Not a bug unique to this tool while it existed — nixpkgs's own
`replaceDirectDependencies` has the identical gap, for the identical reason
(the new path isn't knowable from Rust/Nix-language code until the
derivation exists) — just one neither tool needed to actually hit, since
the fix only needed `$out`, already available for free inside the sandbox.

One more real wrinkle surfaced by this: `nix derivation add` requires the
`.drv`'s output path (and any `env` var that mirrors it) to already match
Nix's own computed hash for that derivation — a hash with no public API
outside Nix's C++ implementation. Rather than reimplement
`hashDerivationModulo`, this tool submits a syntactically-valid placeholder
output path, reads the *actual* value back out of `nix derivation add`'s own
error message (`derivation has incorrect output '...', should be '...'`),
patches it in, and retries — safe only because Nix itself validates the
correction each time. The exact same trick is reused by `--edit`'s drv-edit
mechanism (see below), which has the identical problem for a different reason (there, the
user edited the derivation's `env`/`args` directly).

Reusing the original item's name-version suffix for a graft's scratch
directory (and thus its resulting store item) means every graft this tool
performs automatically satisfies the same-length constraint for *its own*
basename when something further up the closure needs to reference it in
turn — only the user-supplied top-level `--override` pairs need the explicit
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

`--rebuild` controls only propagation *above* whatever a transform flag
produced — the leaf action itself (the file edit, or the initial rebuild
`--edit`'s drv-edit/nix-edit mechanisms already do to produce their own `new`) is
unaffected either way (see §14).

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

So `replace` takes two more path lists — shared by every transform flag,
since `--edit` (whichever mechanism it detects) funnels its produced pairs
into the same walk (§14) — mirroring nixpkgs's `replaceDependencies`'s
`cutoffPackages` option:

- **`--cutoff <path>`** (repeatable): never touch this path, no matter what
  changed beneath it — not even a graft. `classify` returns it unchanged
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
`--override <old> <new>` target wins over `--cutoff`, which wins over the
default strategy (`--rebuild` or plain grafting) — the same order nixpkgs
documents for `cutoffPackages` vs. `replacements` in `replaceDependencies`.

Implementation note: `classify`/`would_change` took on enough
strategy-related parameters (`nix_args`, `full_rebuild`, `cutoffs`,
`force_rebuild`, plus `explicit`) that clippy's `too_many_arguments`
flagged `replace()`'s signature — the actual trigger for introducing
`ReplaceOptions`, a small struct bundling everything about *how* to
replace, separate from *what* to replace. `replace()` takes it as one
argument instead of five-to-seven; the `edit_*::produce_pair` functions
(§14) don't need it at all, since producing a pair has nothing to do with
how it's later propagated.

### Editing capability: producing `(old, new)` from a small change

Rather than requiring a pre-built replacement, three more flags let you
make a small edit and have the tool derive `(old, new)` itself, then hand
it to the exact same `replace` engine (walking the rewrite up through
`C`) — originally three separate `edit <kind>` subcommands, then three
separate `--override-{file,drv,nix}` flags, now one `--edit <path>
<selector>` flag that detects which of the three mechanisms below applies
from what `path` itself is; see §14/§15 for why and how. Whichever
mechanism applies also takes a trailing `-- <extra nix args>` (threaded
through regardless of which one `--edit` picks), forwarded to its own
build step (drv-edit's and nix-edit's initial rebuild; file-edit has none
of its own) and to every graft built on top of it via `replace`.

**File-edit** (`path` resolves to a plain output) — dump `path`, restore
to a scratch directory, open `$EDITOR` on `selector` (or the whole
extracted tree, if `selector` is `.`). No same-length constraint here:
this is an ordinary file edit inside a NAR, which encodes explicit
lengths per entry — unlike the pure reference-swap case, content can grow
or shrink freely.

This mechanism originally stopped at `--add-fixed --recursive` (the
simpler, ultimately-rejected approach from the `replace` design discussion
above) and inherited its reference-registration gap wholesale: an edit
that introduced or preserved a reference to another store path produced a
store item with no recorded reference to it at all, silently. Fixing that
properly means exactly what the `replace` discussion above said it would
— generating a sandboxed-build recipe, not just `nix store add` — just for
an *arbitrary* user edit instead of a known `(old, new)` substitution,
which is what made it a meaningfully bigger problem at the time: there's
no fixed set of candidate references to declare, since the edit can
introduce a reference to anything.

The actual fix turned out not to need a different KIND of recipe, just
one extra step. `store::scan_references` does what the problem needed all
along: a byte-level scan of the edited tree for anything that looks like
`/nix/store/<hash>-<name>`, keeping only the ones that actually exist.
Nix's own post-build scan never searches the whole store — it only ever
matches a build's *declared* inputs — so once the edited tree is staged
into the store (`--add-fixed --recursive`, same as before, now purely an
intermediate: sandboxed builds can only see declared store-path inputs,
never an arbitrary host directory, so this step still has to happen
*somewhere*), a *second* pass — the same dump/restore recipe
`replace::graft_recipe` already uses, with the staged path plus every
`scan_references` hit declared as `inputSrcs` — gives the daemon a correct
candidate set to register them against. Verified by editing a fixture
that starts with zero references to embed one and confirming it's
registered afterward
(`edit_registers_a_reference_the_edit_itself_introduces`), where the old
implementation would have silently produced zero regardless of what the
edit actually embedded.

**Drv-edit** (`path` resolves to a bare `.drv`) — `nix derivation show
<path>`'s deriver returns `{"derivations": {"<basename>.drv":
{...ATerm-JSON...}}}`; the inner object (minus the `derivations` wrapper)
is exactly what `nix derivation add` accepts back — confirmed by
round-tripping an unmodified derivation and getting the identical `.drv`
path back. After a `$EDITOR` session on that inner JSON, adding it back
will almost never succeed on the first try: Nix validates that
`outputs.<name>.path` (and any `env` var that mirrors it, e.g. `env.out`)
matches its own computed hash, and *tells you the correct value* in the
error (`derivation has incorrect output '...', should be '...'`) rather
than computing it for you. There's no public API for this hash
(`hashDerivationModulo`) outside Nix's own C++ implementation, so this
tool does the pragmatic thing: patch the JSON fields the error names,
retry, repeat until `nix derivation add` succeeds or an unrecognized error
appears. This is a real hack, called out here and in the code — it's safe
only because Nix itself is the one validating our guess each time. Once
added, `nix build <drv>^out` actually builds the edited derivation
(there's no way around one real, sandboxed rebuild here — the recipe
changed, not just a reference) and its output feeds `replace` as `(old,
new)`.

**Nix-edit** (`path` is a `.nix` file sitting on disk) — the one place
this tool does use Nix-language evaluation, deliberately scoped to a
single attribute rather than nixpkgs's whole-closure IFD walk.
Best-effort: records the installable's current output path (if it has
one), opens `$EDITOR` on the backing `.nix` file (for `file.nix#attr`
this opens `file.nix` as a whole — there's no attempt to seek to `attr`'s
exact location), then runs `nix build <installable> --no-link
--print-out-paths` to get the new output. Feeds the same `(old, new)`
into `replace`.

**SONAME warning.** Guix's manual is explicit that grafting a shared
library requires matching `SONAME`, and that checking this is the
caller's job — grafting doesn't change that, but it can at least stop
leaving the caller to find out the hard way. `derivation::
warn_on_soname_mismatch` runs once per final `(old, new)` pair in
`collect_pairs` (not inside the recursive walk — the pair itself is what
changed, not every downstream node that happens to depend on it, so
checking once avoids repeating the same warning for every propagated
graft above it). It looks only at `lib`/`lib64` directly under each side
(not a full recursive walk — covers the common case cheaply), matches
`.so`/`.so.N` files by filename between old and new, and shells out to
`readelf -d -W` to pull each one's `SONAME` entry straight out of its
`.dynamic` section. A mismatch prints a warning, never fails the graft:
Guix's own stance is that this is advisory, and `readelf` isn't even on
the packaged binary's wrapped `PATH` (dev-shell only, like
`nix-diff`/`diffoscope` — diagnostic tooling, not core functionality), so
a missing `readelf` has to be a silent no-op, not an error, for the
packaged binary to work at all. Verified with two real `.so` files built
via `gcc -Wl,-soname,...` with deliberately different SONAMEs, confirming
the warning fires and names both — and, implicitly, that it doesn't fire
for anything that isn't a shared library at all, since every other test
in this suite never shows one.

## 5. Known caveats

- **Blind text substitution can corrupt unrelated data.** If a store path's
  basename happens to appear as a substring inside binary data that *isn't*
  actually a store reference, this tool (like Guix, like nixpkgs) will
  rewrite it anyway. Store path basenames are high-entropy 32-character
  hashes, so collisions are astronomically unlikely, but this is a
  fundamentally unverified rewrite, not a semantically-aware one.
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
- **Verified against a real nixpkgs package, round-trip.** Every other
  fixture, including the ELF one above, is built from scratch inside the
  test suite — never an existing, independently-maintained nixpkgs
  derivation with its own real build system and real transitive closure.
  `replace_grafts_a_real_nixpkgs_package_then_ungrafts_it_back` grafts
  `pigz` (a real nixpkgs package) against two real builds of `zlib` that
  differ only in `ZLIB_VERSION` (a version-banner macro, not an
  ABI-affecting one — same `SONAME`, same `libz.so.1` symlink, same
  exported symbols), runs the grafted binary through an actual
  compress/decompress round trip to prove it dynamically links the new
  library correctly (not just that it starts), then grafts the result
  *back* to the original `zlib` and asserts the ungrafted package's NAR is
  byte-identical to the pre-graft original — confirming grafting is a
  reversible, lossless operation on something real, not just on this
  project's own synthetic fixtures.
- **Still no full ABI check.** `derivation::warn_on_soname_mismatch` (see
  §4) covers `SONAME` specifically, the one piece Guix's manual calls out
  by name and the one piece a cheap `readelf -d` comparison can actually
  answer; true ABI compatibility (symbol versioning, struct layout, etc.)
  is a much harder problem this tool makes no attempt at — that's still on
  the caller.
- **`--rebuild` can still fail to find a substitution point, though now it
  says so.** It only recognizes a dependency as a whole `inputs.drvs`/
  `inputs.srcs` entry (matched by output name, see §6) or a literal
  substring of an `env`/`args` string. A dependency referenced some other
  way — most concretely, embedded only *transitively* through another
  declared input's own build output — won't be found, and `rebuild_path`
  now errors instead of silently rebuilding an unmodified derivation (see
  §6). Multi-output derivations are fully supported as of §6, which used to
  be a separate, harder limitation here.
- **No substituter trust story.** Every grafted or rebuilt path here was
  produced locally; no binary cache has ever seen it, so there's nothing that
  will substitute it elsewhere, and no mechanism here to sign or publish one.
- **The `nix derivation add` output-path retry loop (§4) is inherently
  fragile** — it depends on the exact wording of two specific Nix error
  messages, shared by `replace`'s synthetic-derivation construction and by
  `--edit`'s drv-edit mechanism. If a future Nix version changes that wording, the loop just
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
(reusing `editor::edit`, already shared by `--edit`'s file-edit/drv-edit mechanisms), and
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

Same fix applied in both places: explicit `--override` targets (the path(s)
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

Every earlier example required both sides of `--override` (and
`closure-root`) to already be built store paths — meaning a first-time user
had to run `nix build` twice themselves, by hand, before graft could do
anything. That's a real ergonomics gap: Guix users never see a hash, since
grafting is driven by a `replacement` field on a package, not a CLI argument.
`src/installable.rs`'s `resolve` closes most of that gap without touching
Guix's actual mechanism (a declarative field) — it just lets every
path-taking CLI argument (`closure-root`, both sides of `--override`,
`--edit`'s `path`) accept anything `nix build` itself accepts (a
flake reference, a `.drv` path) or a legacy `file.nix`/`file.nix#attr`
expression installable, building it via `derivation::nix_build` if it isn't
already realized.

One subtlety this had to get right: a store-path-*shaped* string (starts
with `/nix/store/`) is returned completely unvalidated — not canonicalized,
not existence-checked, not run through `nix build` — deferring entirely to
whatever the caller already does with it. Two reasons this matters, both
caught by the existing test suite rather than reasoned out in advance:

- **`replace`'s syntax-before-existence ordering.** `replace()` deliberately
  checks `--override`'s basename-length constraint *before* requiring either
  side to exist (see §4/caveats — this is what lets it reject a bogus pair
  fast, without touching the store). `--override old old-longer-name` where
  `old-longer-name` was never built is exactly this test case: resolving it
  eagerly (via `nix build` or even just `canonicalize`) turns a clean
  "basenames differ in length" rejection into a confusing "don't know how to
  build this path" error, since the existence check now happens *before*
  `replace()` ever gets to run its own syntax check. Passing store-path-shaped
  strings through untouched preserves the original ordering exactly.
- **`--edit`'s bare-`.drv` + output disambiguation.** A bare `.drv`
  path for a multi-output derivation, combined with `--edit`'s second
  value (the output name, or `.` to infer), is meant to let
  `edit_drv::produce_pair` pick a specific output *after* the fact via
  `derivation::locate_output`. Running that `.drv` path through `nix build`
  during resolution would both build it prematurely and collapse the
  disambiguation this flag exists for (a bare `nix build <drv>` with no
  `^output` builds every output, not the one you asked for). Verified
  directly against the `multiOut` fixture: `--edit <drv> extra` still
  resolves the `extra` output specifically, not `out`.

Everything that isn't store-path-shaped goes through two branches: a
`#`-split first component that exists on disk and ends in `.nix` is treated
as a legacy `-f file.nix [attr]` installable (matching `edit_nix.rs`'s own
long-standing installable parsing, which `resolve` doesn't replace —
`edit_nix::produce_pair` still has its own narrower parser, since it distinguishes "already
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
`replace()` now warns explicitly when an `old` from `--override` isn't in
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

Each node's row is computed fresh from the same `depends_on` edges the
graph already draws (`1 +` the deepest row among its own included direct
references) — deliberately *not* `classify`'s scheduling `level` (§10).
The two look like the same question ("which nodes could build at the same
time" vs. "which nodes belong on the same row") but aren't: that level is
always `0` for `Explicit`/`Cutoff` nodes, since neither needs a build to
wait for, which put every one of them on the graph's bottom row regardless
of how deep they actually sat in the reference graph — a cutoff partway up
a chain rendered *below* the dependency it left untouched. Only
`Unchanged` is excluded — a real closure's unchanged majority is genuine
noise for a report meant to answer "what did this graft do," not a graph
of the whole closure. `Cutoff` is deliberately *kept*, even though it's
never built: cutting a path off is a decision the caller made on purpose
(`--cutoff`/`--force-graft`/`--force-rebuild` are exactly "override the
default here"), not nothing happening — a report that silently dropped it
would hide the one place propagation was stopped on purpose, which is
precisely the kind of thing a report should be confirming actually worked.
Shown in gray, old and new paths identical (it really does resolve to
itself), with no diff generated against itself even under `--report-diff`.
Edges are drawn only between two *included* nodes (a node's direct
references that also made the cut), so the graph shows the actual "story"
subgraph, not a mess of everything each node happens to depend on.

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

## 13. Feeling like `nix` itself: common flags promoted out of `-- <nix args>`

Every build this tool does already forwards a trailing `-- <nix args>`
verbatim to the underlying `nix build` call, so nothing here was ever
*impossible* — `graft replace foo --override a b -- -L --max-jobs 4` always
worked. But it meant knowing in advance which flags were "ours" (before
`--`) versus "nix's" (after it), which is exactly the kind of thing someone
coming from `nix build` shouldn't have to think about for the handful of
flags they reach for constantly.

`NixPassthroughArgs` (`main.rs`) lists the common ones under their real
`nix` names and shorts — `-L`/`--print-build-logs`, `-j`/`--max-jobs`,
`--cores`, `--builders`, `--option <name> <value>`, `--impure`, `--offline`,
`--refresh`, `-k`/`--keep-going`, `--fallback`, `--show-trace` — flattened
into `StrategyArgs` so every subcommand gets them automatically, same as
`--cutoff`/`--out-link`/etc. already are. `--option` takes two separate
values (`--option keep-going true`), matching nix's own two-value form
exactly — the same convention `--override` itself was moved to in §14, for
the same reason: consistency with `nix` wins over this project's own prior
`name=value` habit, since the whole point is forwarding exactly what `nix`
itself expects.

`StrategyArgs::merged_nix_args` renders these back into their equivalent
CLI tokens and prepends them to whatever trailing `-- <nix args>` the
caller also supplied, so both work together: the promoted flags cover the
common case, and `--` remains the escape hatch for anything more obscure
(`--eval-store`, `--override-flake`, etc.) without this tool needing to
know about every flag `nix` has. The merged result is used everywhere
`nix_args` already flowed — installable resolution, the actual
graft/rebuild builds, and `--out-link`'s own `nix build --out-link` call,
which previously took no `nix_args` at all.

## 14. Unifying `replace` and `edit` into one combinable command

`replace` and the three `edit <kind>` subcommands were always doing the
same thing underneath — produce an `(old, new)` pair, then hand it to the
exact same closure walk (§4) — differing only in *how* the pair gets
produced: `--override` takes it ready-made, `edit file` dumps/edits/re-adds
a file, `edit drv` edits a derivation's JSON and rebuilds, `edit nix`
edits a `.nix` source and rebuilds an attribute. Four subcommands for one
underlying operation meant picking exactly one of them per invocation,
even though nothing about the engine required that — a real ergonomics
gap, not just a cosmetic one, since it meant "replace this dependency
*and* hand-patch that config file, in the same closure" required two
separate `graft` runs with two separate out-links to reconcile yourself.

Collapsed into flags on `replace` itself, each repeatable and freely
combinable in one invocation. `main.rs`'s `collect_pairs` is the one
place they all converge: it resolves every occurrence into `(PathBuf,
PathBuf)` pairs and returns the combined list, which `replace()` then
walks exactly as it always has — no change to the engine itself, only to
how pairs reach it. `graft nixos-system` gets the same flags for the same
reason (`TransformArgs`, flattened into both `Cmd::Replace` and
`Cmd::NixosSystem`).

This went through two rounds of naming before settling (see §15 for the
second, bigger one): originally four flags —
`--override`/`--override-file`/`--override-drv`/`--override-nix` — one
per mechanism. Two design choices applied to that four-flag shape, both
still true of the two-flag shape §15 replaced it with:

- **Moved to `nix`'s own two-separate-values convention**
  (`--override <old> <new>`, not `--override <old>=<new>`) instead of
  this project's prior `name=value` habit — the same motivation as §13's
  `--option`. For the file/drv-editing flags specifically, this also
  sidesteps a real ambiguity: a *repeatable* flag with an *optional*
  second value (the old `edit file <root> <path> [<subpath>]` positional
  shape) can't be expressed as a flat, repeated multi-value `clap` arg
  without losing which values belong to which occurrence. Fixed arity
  avoids that entirely, at the cost of a sentinel for "no second value
  really needed": `.` means "the whole tree" for a subpath, and "infer
  the output automatically" for an output name (which only works if
  there's exactly one, same as leaving `--output` off before).
- **The `edit_*` modules' `run` functions were split into `produce_pair`.**
  Previously each one produced its `(old, new)` pair *and* called
  `replace::replace` itself, taking a full `ReplaceOptions` just to pass
  it through. Now that several of these can combine with `--override` and
  each other in one invocation, pair-production has to finish for all of
  them *before* `replace()` runs once over the combined list — so
  `produce_pair` only does the former, dropping `ReplaceOptions`/
  `closure_root` entirely where they were only ever used to call
  `replace()` at the end. `edit_file::produce_pair` keeps a `closure`
  slice parameter (computed once in `collect_pairs`, not once per
  occurrence) for its membership check, since that's the one piece of
  per-call state editing a file still genuinely needs.

Verified with a new test combining `--override` with a file edit against
two independent branches of the same closure in one invocation
(`replace_and_edit_combine_in_one_invocation_against_independent_nodes`),
confirming both effects land in the final grafted result together — the
one thing that was structurally impossible before this section's change.

The flag itself was originally called `--replace`, which read awkwardly
once it was a flag *on* a `replace` subcommand (`graft replace --replace
...`) rather than the whole operation's name. Renamed to `--override`,
matching what `nix`'s own `--override-input` already calls "swap this
specific thing for that one" — `graft replace --override old new` reads
the way the other transform flags already did (a stated means of
producing a pair, not a repeat of the subcommand's own name). Pure rename;
`replace`'s own module/function/subcommand names were deliberately left
alone, since the *operation* is still accurately called replace — only
the one flag that used to share its name changed.

The other three transform flags went through the same move for the same
reason: `--edit`/`--edit-drv`/`--edit-nix` became `--override-file`/
`--override-drv`/`--override-nix`, so all four shared one `--override*`
family — exactly the pattern `nix` itself already uses for
`--override-input`/`--override-flake` (two different kinds of override,
one shared prefix). Pure rename again; `edit_file`/`edit_drv`/`edit_nix`'s
module and `produce_pair` function names stayed put, since "edit" is still
an accurate description of *how* each one derives its pair — only the
flags needed to read as a family. This four-flag shape didn't last — see
§15 for why it collapsed further, down to two.

## 15. Down to two flags: `--override` stays dumb, `--edit` detects its own kind

Four flags for one operation was already an improvement over four
subcommands, but it's still four names to remember for something that
only ever splits two ways: either you already have the `new` side
(`--override`), or you want `graft` to derive it by editing one specific
thing (everything else). The three editing mechanisms never needed to be
*separate flags* — they needed telling apart, and what tells them apart
is entirely a property of the one thing being edited, never anything
about a second value. That observation collapses `--override-file`/
`--override-drv`/`--override-nix` into one `--edit <path> <selector>`,
with `detect_edit` (`main.rs`) choosing the mechanism:

1. `path` is a `.nix` file sitting on disk (not a store path) → nix-edit.
2. `path` resolves to a bare `.drv` → drv-edit.
3. Otherwise → file-edit.

Each check is cheap, local, and — critically — *decisive*: a given `path`
is always exactly one of these three, never ambiguously more than one, so
`detect_edit` never has to guess and backtrack. `selector` plays whatever
role is appropriate for the mechanism it ends up being (attribute, output
name, or subpath), but never influences *which* mechanism gets chosen.

**Why `--override` doesn't get the same treatment.** The first draft of
this section tried to make bare `--override <a> <b>` auto-detect too —
same four mechanisms, chosen from the *shape* of `(a, b)` together (does
`b` look like an existing subpath of `a`? an output name? else plain
pair). It shipped a real bug, caught by actually running it before
committing to the design (not by reasoning about it): `Path::join`
discards its base and returns the argument unchanged when that argument
is absolute. Every ordinary `--override old new` call passes an
already-built absolute store path as `new` — so `resolved_a.join(b)`
collapsed to just `b`, which (being a real, already-built path) trivially
"existed", misdetecting *every single plain override* as a file-edit and
opening `$EDITOR` on a non-interactive test harness with no TTY, which
hangs forever rather than failing fast. The bug was fixable (check `b`
isn't absolute before treating it as a candidate subpath), but it's a
symptom of a structural difference worth not papering over: `--edit`'s
two values describe *one thing* (what to edit, and a selector *within*
it), where neither value's interpretation depends on inspecting the
other. `--override`'s two values describe *two independent, unrelated
things* (old and new) that happen to need telling apart from three other
*unrelated* mechanisms — and the only available signal for that was
inspecting `b`'s shape, which is exactly the kind of cross-value
inference that produces exactly this kind of false positive. `--override`
reverted to simply resolving both sides as installables, no detection,
exactly as before the attempt.

**Preserving `--override-drv`'s old convenience.** The explicit
`--override-drv <path> <output>` flag let you point at an ordinary,
already-built output path (not its `.drv`) and still edit its derivation
— the output name is unambiguous already for a plain output, so it was
simply ignored. Folding this into `--edit`, where a plain output
unconditionally means file-edit, would have quietly dropped that
capability: there'd be no way left to say "edit this path's *derivation*"
without first manually resolving its `.drv` yourself. `derivation::
has_output(path, name) -> bool` (a thin, failure-tolerant wrapper around
the existing `deriver_of`/`show`/`locate_output`) restores it as a
narrow, safe fallback inside `detect_edit`: when `path` is a plain output
and `selector` *isn't* an existing subpath inside it, check whether
`selector` instead names one of `path`'s own deriver's outputs — if so,
drv-edit, resolving the deriver automatically, exactly as before. Subpath
wins when both would apply, since it's the more literal reading of "edit
this path". This differs from the rejected `--override` design in a way
that matters: the fallback only ever fires for a *single* path's own
properties (does its deriver have this output?), never by trying to
resolve `selector` as some unrelated installable and seeing if that
happens to succeed — there's no absolute-path collision to be had here,
since nothing here is ever compared against a second, independently
chosen value.

Verified end-to-end for all three mechanisms via `--edit` directly
(`edit_file_grafts_a_hand_edited_file_up_through_the_closure`,
`edit_drv_resolves_a_plain_output_path_to_its_deriver_automatically`
— now exercising the `has_output` fallback specifically, selecting by
`path`'s own output name "out" rather than `.` — and the new
`edit_detects_a_nix_file_on_disk_and_rebuilds_the_attribute`, using a
dedicated scratch `.nix` file rather than the shared fixture, since
nix-edit opens `$EDITOR` on it in place).

## 16. Multi-output packages at once: `^*`

Every example so far pairs exactly one output of `old` with one output of
`new`, or edits exactly one output of a derivation at a time. For a
multi-output package (`openssl` with `out`/`dev`/`bin`/`man`, say)
replaced wholesale, that meant one `--override` per output the closure
actually references — tedious and easy to under-cover (miss an output
something downstream turns out to need). Nix already has a name for "all
outputs of this installable": `pkg^*`. Rather than invent graft-specific
syntax for the same idea, both flags just recognize it.

**`--override old^* new^*`** (`main.rs::resolve_override`) — requires
both sides to use `^*` together (a bare `old^*` paired with a bare `new`
doesn't have a sensible reading, so it's rejected rather than guessed
at). `derivation::resolve_outputs` runs `nix build <installable> --json`
on each side (the same JSON shape `build_many` already parses, just for
one installable instead of several `drv^output` targets) and returns
every `(name, path)` it has. Outputs are paired by *name*, not position —
`old`'s `dev` with `new`'s `dev`, regardless of what order either side's
JSON happened to list them in. An output present in `old` but missing
from `new` is skipped with a warning, not a hard failure: nothing in the
closure may ever reference that specific output anyway, so failing the
whole operation over it would be punishing a case that was never actually
a problem.

**`--edit <path>^* .`** (`edit_drv::produce_pairs_all_outputs`) — checked
first in `detect_edit`, before any of the ordinary kind-detection logic,
since `^*` already says unambiguously what's wanted. One edit session
(`$EDITOR` on the derivation's JSON) covers every output automatically —
a derivation's `env`/`builder`/`args` are shared across however many
outputs it declares, so there's exactly one recipe to edit regardless of
output count. What's new is building all of them afterward: `derivation::
all_outputs` reads every `(name, path)` straight out of `nix derivation
show`'s JSON (no `nix build` needed just to enumerate them, unlike the
installable-level `resolve_outputs` above, which has no equivalent
"inspect without building" option), then one `derivation::build_many`
call realises every output of the *edited* derivation together — the
same per-level batching `replace()` itself already uses, reused here for
the same reason: one `nix build` call, not N.

`selector` isn't meaningful in either `^*` form (there's no single
output/attribute/subpath left to name), and must be `.` — checked
explicitly rather than silently ignored, consistent with how a mistyped
flag elsewhere in this tool fails loudly instead of doing something
quietly different from what was asked.

Verified against a dedicated two-output fixture pair
(`multiAllOld`/`multiAllNew`/`multiAllConsumer`, both outputs genuinely
referenced via symlinks so a real closure walk exercises the pairing) for
`--override`, and against the existing `multiOut` fixture for `--edit`,
checking the *un*-edited output (`out`) was still rebuilt alongside the
edited one (`extra`) from the one session — `out` and `extra` are
independent sibling outputs with no reference between them, so the only
way to find `extra`'s rebuilt path for inspection is the `-v` log line
reporting it directly, not anything reachable by walking references from
`out`.

## 17. Batching `nix path-info` for the whole closure, not one call per path

§5 used to list this as a known caveat: `classify`/`would_change` recursed
via a single-path `store::references(path)`, one `nix path-info --json`
subprocess per closure member. Fine for the hand-built fixtures this tool
tests against (dozens of paths), not for a real NixOS closure (thousands).

`nix path-info --json` already accepts more than one installable and
returns one JSON object keyed by path, so there was no actual need to call
it more than once per `replace()` invocation — this wasn't a case of
needing a different Nix primitive (the `exportReferencesGraph`-style bulk
query §5 used to speculate about), just calling the existing one correctly.
`store::references_many(paths: &[PathBuf]) -> Result<HashMap<PathBuf,
Vec<PathBuf>>>` replaced the single-path version, computed once in
`replace()` right after the closure itself, and threaded through as a
precomputed map lookup everywhere the old code called `store::references`
live: `classify`, `changed_refs_of`, `build_recipe`, `would_change`,
`changed_direct_refs`, `interactive_select`, `build_report_nodes`. Several
of these had no other fallible operation left once the live subprocess call
was removed, so they dropped their now-unnecessary `Result`/`?` plumbing
too — a sign the change was a pure hoist, not a behavior change.

## 18. Replacing by package name: `--override-name` and `graft find`

§8 let `--override` accept installables instead of requiring a store path
already in hand, but that still means knowing *which* installable produces
the exact thing already in the target closure. Guix users never see a hash
at all — grafting there is driven by a `replacement` field on a package.
`--override-name <name> <new>` is the ergonomic next step: `name` is a bare
package name (or `name-version`), resolved by searching the closure itself.

The unsafe version of this feature would be "match by name, replace every
match" — wrong whenever a closure genuinely contains two different things
under the same name (two incompatible versions coexisting on purpose, most
concretely), since replacing both with the same `new` would silently apply
a change to something that was never asked for. So resolution is scoped
tightly: `store::find_by_name(closure, query)` matches on `(pname,
version)`, parsed from a store path's `name-version` suffix the same way
Nix's own `parseDrvName` does (`store::parse_name` — the version starts at
the first `-` immediately followed by a non-letter; this is also where Nix
folds a non-`out` output's suffix into the "version" half, e.g.
`openssl-3.2.1-dev` parses to `("openssl", "3.2.1-dev")`, so a query that
includes the suffix disambiguates a specific output for free, no separate
concept needed). `resolve_override_name` (`main.rs`) requires *exactly one*
distinct path to match; zero or more than one is a hard refusal, printing
every candidate store path rather than guessing — the same shape as the
`^*`-pairing warn-and-skip in §16, except here ambiguity is a correctness
risk (which one did you mean?) rather than a "safe to just not touch it"
case, so it fails instead of warning.

`graft find <closure-root> <name>` is the read-only counterpart — same
`find_by_name` search, printing each match with one direct consumer (found
via the same `references_many` map §17 introduced, inverted) so "which
occurrence is this" has an answer before committing to an override. Zero
risk, since it only searches and prints.

Verified against `nameMatchDep`/`nameMatchDepV2`/`nameMatchConsumer` (a
single unambiguous match, successfully overridden and run) and
`nameMatchDupA`/`nameMatchDupB`/`nameMatchDupConsumer` (two distinct store
paths sharing one name, both referenced by the same consumer via symlink —
`--override-name` must refuse listing both, `graft find` must list both).
