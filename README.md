# graft

A Guix-style graft/rewrite tool for the Nix store: rewrite one dependency
deep in an already-built closure and have everything built on top of it
*replayed* onto the new base — the same shape as `git rebase`, applied to
a store closure instead of a commit history, without actually rebuilding
anything from scratch.

Nix has the same low-level technique in nixpkgs
([`replaceDependencies`](https://github.com/NixOS/nixpkgs/blob/master/pkgs/build-support/replace-dependencies.nix)),
but only as an opt-in Nix-language library call that requires
import-from-derivation. `graft` does the same thing directly against the
Nix daemon/store (`nix build`, `nix store`, `nix path-info`, `nix
derivation`) — no Nix expression or IFD required, so it works on *any*
already-realized closure, including one you have no expression for (e.g. a
live system profile).

See [DESIGN.md](DESIGN.md) for the full write-up: how Guix's grafts work,
what nixpkgs already has, why this exists, and the tradeoffs of each mode.

## Building

```
nix build       # produces ./result/bin/graft
nix develop     # dev shell with cargo/rustc/clippy + nix/bash/gnused on PATH
```

Tests drive a real Nix daemon and can't run in a build sandbox, so run them
via the dev shell instead of `nix build`'s checkPhase:

```
nix develop -c cargo test
```

## The basic idea

```
graft replace <closure-root> --override <old> <new>
```

Walks the closure rooted at `<closure-root>` bottom-up and replays every
path that transitively depends on `<old>` onto `<new>` instead — the same
thing `git rebase` does to every commit after the one you rewrote, except
each "commit" here is a store path, and replaying it means a blind but
fast NAR byte-substitution (the same technique Guix and nixpkgs use)
rather than actually recompiling it. Anything that never depended on
`<old>` is left alone entirely, same as commits before the rebase point
never move. Paths with no data dependency on each other graft concurrently
rather than one at a time. Reports each one as it happens, plus a closing
summary in the same spirit as Guix's own grafting output:

```
grafted /nix/store/9f3a...-openssl-3.2.1 -> /nix/store/2b0e...-openssl-3.2.2
grafted /nix/store/7c1d...-curl-8.9.0 -> /nix/store/a84f...-curl-8.9.0
2 grafted, 1 explicit replacement, 54 unchanged (57 total)
/nix/store/a84f...-curl-8.9.0
```

`<closure-root>` and both sides of `--override` are installables, not just
store paths — a flake reference, a `.drv` path, a `file.nix`/`file.nix#attr`
expression, or a plain store path, built automatically if it isn't already:

```
graft replace .#myImage --override nixpkgs#openssl nixpkgs#openssl_3_2
```

## Editing instead of replacing

`--edit <path> <selector>` lets you make a small edit and have `graft`
derive the `old`/`new` pair for you, rather than supplying a pre-built
one. It figures out *how* to edit `path` automatically, from what `path`
turns out to be — you don't pick a mode up front:

- `path` a `.nix` file sitting on disk → opens `$EDITOR` on that source,
  rebuilds the attribute `selector` names (or the whole file if `selector`
  is `.`), grafts the new output up.
- `path` a bare `.drv` → opens `$EDITOR` on its derivation JSON
  (`env`/`builder`/`args`), does one real sandboxed rebuild. `selector` is
  the output to edit when there's more than one (`.` to infer it, which
  only works if there's exactly one).
- Anything else → dumps `path`, opens `$EDITOR` on `selector` inside it
  (`.` for the whole tree), re-adds the edited tree.

```
$ graft replace $CONSUMER --edit $CONSUMER bin/consumer
1 explicit replacement, 6 unchanged (7 total)
/nix/store/v5qzm6...-consumer

$ cat /nix/store/v5qzm6...-consumer/bin/consumer
#!/nix/store/.../bash
/nix/store/.../dependency/bin/dependency

patched locally        # whatever you changed in $EDITOR
```

`--edit` and `--override` may both be repeated and freely combined in a
single invocation, grafting every result up through the closure together
in one pass — useful when the two changes are unrelated (one dependency
swap, one hand-edited file) and you'd rather not reconcile two separate
runs' out-links yourself:

```
graft replace /run/current-system \
  --override nixpkgs#openssl nixpkgs#openssl_3_2 \
  --edit "$(readlink -f /etc/foo.conf)" .
```

## Multi-output packages: `^*`

Both flags understand nix's own `^*` ("all outputs") selector, so a
multi-output package doesn't need one call per output:

```
graft replace .#myImage --override "nixpkgs#openssl^*" "nixpkgs#openssl_3_2^*"
```

Resolves every output of each side and pairs them up by name (`out` with
`out`, `dev` with `dev`, and so on) — an output present on one side but
missing on the other is skipped with a warning, not a hard failure.

`--edit <drv>^* .` does the same for editing: one `$EDITOR` session on the
derivation's JSON still changes the recipe for every output at once, so
`^*` there just means "give me a pair for each of them" instead of making
you pick one.

(`--edit`'s `path` needs the real store path, not an `/etc` symlink to it
— `readlink -f` resolves that, the same way you'd find it to inspect it
manually.)

## Flags

Strategy — how propagation above a change is handled:

- `--dry-run` — report what would happen without touching the store.
- `--rebuild` — do a real sandboxed rebuild instead of grafting (mirrors
  Guix's `--no-grafts`); no equal-length-basename constraint, but every
  affected path needs a known deriver.
- `--cutoff <path>` (repeatable) — never touch this path, no matter what
  changed beneath it; propagation stops there. Use for anything a blind
  substitution could corrupt (e.g. a NixOS closure's embedded store
  database).
- `--force-rebuild <path>` / `--force-graft <path>` (repeatable) —
  override the graft/rebuild strategy for one specific path.
- `-i`/`--interactive` — open `$EDITOR` on a `git rebase -i`-style list of
  every affected path and its strategy; edit the words, save, apply.

Output and provenance:

- `-o`/`--out-link <path>` — create a GC-root symlink at `path` pointing
  at the result, like `nix build -o`. Every build here otherwise passes
  `--no-link`, so without this the result isn't protected from a
  concurrent garbage collection. Also appends one line to
  `<path>.graft-history.jsonl` (timestamp, exact command, resulting path)
  — `--out-link` only ever points at the *latest* generation, so this is
  what makes "what did I graft into this last week" answerable afterward.
- `--report <dir>` — write `<dir>/index.html`: a dependency graph (colored
  by strategy) plus a details table of everything grafted, rebuilt, or
  explicitly replaced. Self-contained, no CDN. Works under `--dry-run` too.
- `--report-diff` (needs `--report`) — embed `nix-diff` (why an explicit
  replacement or rebuild differs — not shown for plain grafts, since
  grafting never changes the derivation) and link each node's `diffoscope`
  artifact diff. Needs `nix-diff`/`diffoscope` on `PATH` (in this project's
  own `nix develop` shell already).

The common `nix build`/`nix eval` flags are available directly, under
their real `nix` names and shorts, forwarded to every underlying `nix
build` call — no need to remember which flags are "ours" vs. "nix's":

- `-L`/`--print-build-logs`, `-j`/`--max-jobs`, `--cores`, `--builders`,
  `--option <name> <value>`, `--impure`, `--offline`, `--refresh`,
  `-k`/`--keep-going`, `--fallback`, `--show-trace`.
- `-- <nix args>` — anything after a literal `--` is forwarded too, for
  anything not covered above (e.g. `-- --eval-store <url>`).

`graft --version` reports the installed version. Run `graft replace
--help` for the full list with descriptions.

## `graft nixos-system`

Patch the currently running NixOS system without looking up its path
first — same transform/strategy/output flags as `replace`:

```
graft nixos-system --override nixpkgs#openssl nixpkgs#openssl_3_2
```

Defaults to `/nix/var/nix/profiles/system` (override with `--profile`); add
`--switch test|switch|boot` to register the result and actually activate it
via `switch-to-configuration` (same action names as `nixos-rebuild`) —
without `--switch`, it only reports the new path and prints the two
commands you'd run to apply it yourself.

## Examples

Preview what a graft would do, without touching the store:

```
graft replace .#myImage --override nixpkgs#openssl nixpkgs#openssl_3_2 --dry-run
```

Do a real rebuild instead of a byte-level graft, protect the result from
GC, and write an HTML report of what happened:

```
graft replace .#myImage \
  --override nixpkgs#openssl nixpkgs#openssl_3_2 \
  --rebuild --out-link ./result --report ./report
```

Replace two independent dependencies in one pass, forcing one of them to
use the rebuild strategy regardless of the (default-graft) global mode:

```
graft replace .#myImage \
  --override nixpkgs#openssl nixpkgs#openssl_3_2 \
  --override nixpkgs#curl nixpkgs#curl_8_9 \
  --force-rebuild /nix/store/...-curl-8.9.0
```

Never touch a NixOS closure's embedded store database while grafting
everything else above a changed package:

```
graft nixos-system --override nixpkgs#openssl nixpkgs#openssl_3_2 \
  --cutoff /nix/store/...-nixos-system-registration
```

Walk through every affected path interactively, picking a strategy per
node like `git rebase -i`:

```
graft replace .#myImage --override nixpkgs#openssl nixpkgs#openssl_3_2 --interactive
```

Build with full logs streamed live and a pinned job count, forwarding
straight into the underlying `nix build` the same way you'd call `nix
build` itself:

```
graft replace .#myImage --override nixpkgs#openssl nixpkgs#openssl_3_2 -L -j4
```

## Status

A prototype, not a production tool — see DESIGN.md's "Known caveats" section
for what's unverified or unfinished (blind text substitution, no SONAME/ABI
check, no substituter trust story, and a few others).
