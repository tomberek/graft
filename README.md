# graft

A Guix-style graft/rewrite tool for the Nix store: patch a dependency into
an already-built closure without rebuilding everything above it.

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

## Usage

Replace a dependency throughout a closure:

```
graft replace <closure-root> --replace <old-store-path>=<new-store-path>
```

This grafts every affected path bottom-up (a blind but fast NAR
byte-substitution, same technique Guix and nixpkgs use) and prints the new
closure root.

Useful flags (all repeatable where noted):

- `--dry-run` — report what would happen without touching the store.
- `--rebuild` — do a real sandboxed rebuild instead of grafting (mirrors
  Guix's `--no-grafts`); no equal-length-basename constraint, but every
  affected path needs a known deriver.
- `--cutoff <path>` — never touch this path, no matter what changed beneath
  it; propagation stops there. Use for anything a blind substitution could
  corrupt (e.g. a NixOS closure's embedded store database).
- `--force-rebuild <path>` / `--force-graft <path>` — override the
  graft/rebuild strategy for one specific path.
- `-i`/`--interactive` — open `$EDITOR` on a `git rebase -i`-style list of
  every affected path and its strategy, edit, save to apply.
- `-- <nix args>` — anything after a literal `--` is forwarded to the
  underlying `nix build` calls (e.g. `-- -Lv --builders ''`).

Instead of supplying a pre-built `old=new` pair yourself, three subcommands
let you make a small edit and have `graft` derive it for you, then graft the
result up through the closure the same way:

```
graft edit file <closure-root> <path> [<subpath>]   # edit a file in a built output
graft edit drv  <closure-root> <path>                # edit a derivation's JSON and rebuild it
graft edit nix  <closure-root> <file.nix>[#attr]     # edit the .nix source and rebuild it
```

All four commands (`replace` and the three `edit` variants) accept the same
strategy flags above; run `graft <command> --help` for the full list.

## Status

A prototype, not a production tool — see DESIGN.md's "Known caveats" section
for what's unverified or unfinished (blind text substitution, no SONAME/ABI
check, no substituter trust story, and a few others).
