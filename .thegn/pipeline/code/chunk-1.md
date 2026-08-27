# Chunk 1 — Delivery: install completions at package time

**Issue:** THE-36 (right layer for shell completions).
**Design:** `.thegn/pipeline/architect/design.md` — read §1 and §2 first.
**Rust changes: none.** This chunk is Nix + justfile + CI only.

## Why

`thegn completions <shell>` has existed since the CLI-namespaces change and
works fine, but **nothing installs its output**. `nix/package.nix` symlinks the
`tg` alias and wraps PATH; it never calls the generator. `release.yml` ships the
binary plus two licence files. So on every install path today the user gets zero
completions unless they read `docs/cli.md` and wire it by hand.

The design's answer to "which layer owns completions" is: **the packager owns
delivery, the user's shell rc does nothing.** This chunk is that layer. It is
the highest value-per-risk piece of THE-36 and does not depend on any other
chunk — it installs whatever `thegn completions <shell>` emits, so it keeps
working unchanged if chunk 2 later swaps that output from a static `aot` script
to a dynamic registration shim.

## Files you own

- `nix/package.nix`
- `justfile`
- `.github/workflows/release.yml`

Do not touch anything under `crates/`, `docs/`, `openspec/`, or `test/` — other
chunks own those.

## Approach

### 1. `nix/package.nix` — install completion files

Add `installShellFiles` and `stdenv` to the function arguments; add
`installShellFiles` to `nativeBuildInputs`.

In `postInstall`, generate and install completions **after** the `${aliasName}`
symlink is created and **before** `wrapProgram` runs. Order is load-bearing:

- after the symlink, because the generator names the script from `argv[0]` — so
  running the binary through the `tg`/`tg-dev` symlink is how you get the alias'
  script, and there is no other supported way to ask for a specific name;
- before `wrapProgram`, because wrapping replaces `$out/bin/${binName}` with a
  shell wrapper and moves the real binary aside; generating first means you
  execute the plain binary and need none of the wrapped PATH.

Guard the whole block on
`lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform)` —
under cross-compilation the just-built binary cannot run in the sandbox, and the
package must still build (completions are simply omitted there).

**Sandbox hygiene — this will bite you if you skip it.** `thegn completions`
currently dispatches through `run_subcommand`, which calls
`Config::load_layered` and `host_config::merge_db_hosts` (**which opens the
SQLite state DB**) before it reaches the generator. In the Nix build sandbox
`HOME` is `/homeless-shelter`, so export a scratch environment for the
generation commands:

```
export HOME=$TMPDIR
export XDG_STATE_HOME=$TMPDIR/state
export XDG_CONFIG_HOME=$TMPDIR/config
export THEGN_NO_MIGRATE=1
unset THEGN_LOG
```

Then, for each of `${binName}` and `${aliasName}`:

```
installShellCompletion --cmd <name> \
  --bash <($out/bin/<name> completions bash) \
  --zsh  <($out/bin/<name> completions zsh) \
  --fish <($out/bin/<name> completions fish)
```

Notes:

- Only bash/zsh/fish — `installShellCompletion` has no elvish/PowerShell
  destination, and neither has a standard Nix install dir. Elvish and PowerShell
  users keep using `thegn completions <shell>` by hand; that is intentional and
  chunk 3 documents it.
- The dev channel must get its own pair (`thegn-dev`, `tg-dev`) — the names come
  from the existing `binName`/`aliasName` bindings, so writing the loop over
  those two variables handles both channels for free.
- If process substitution (`<(...)`) gives you trouble under the build shell,
  write the scripts to `$TMPDIR` first and pass paths; do not silently drop a
  shell.

Home Manager and nix-darwin need **no** change: `home.packages = [cfg.package]`
already puts `share/zsh/site-functions`, `share/bash-completion/completions` and
`share/fish/vendor_completions.d` on the respective search paths. Verify this
claim rather than assuming it, and if a module wiring turns out to be needed,
say so in your commit message rather than editing `nix/hm-module.nix`
speculatively — it has a drift test (`tests/hm_module_drift.rs`) that will fail
if you add an option that is not a real config key.

### 2. `justfile` — a generation recipe

Add `completions` (place it near `build`, and mention it in the recipe comment
block that lists the build recipes):

- depends on `build`;
- writes `target/completions/{bash,zsh,fish,elvish,powershell}/` for both the
  `thegn` and `tg` names, using the debug binary at
  `target/debug/thegn`;
- for the `tg` name, symlink or copy the binary to `target/completions/tg` and
  invoke _that_, so `argv[0]` resolves to `tg`;
- exports the same scratch `XDG_STATE_HOME` / `THEGN_NO_MIGRATE=1` hygiene as
  the Nix build — this shell often runs _inside a live thegn_ and a recipe that
  opens the real DB is a `CLAUDE.md` violation;
- prints the destination directory when it finishes.

Use `target/completions/` (already covered by `/target` in `.gitignore`) — do
not introduce a `dist/` directory.

Do **not** add `completions` to the `ci` recipe: it is a generation convenience,
not a gate.

### 3. `.github/workflows/release.yml` — an arch-independent asset

The CLI tree has no `#[cfg]`-gated `Command` variants (verified: the only
`#[cfg]` in `main.rs` outside tests is a Windows terminal check inside a
function body), so the generated scripts are **identical across targets**. Ship
them once rather than per-target.

Add a job `completions`:

- `needs: create-release` (the `upload` matrix and this job can run in
  parallel — both only need the draft release to exist);
- `runs-on: ubuntu-latest`;
- checkout with the same `ref:` expression the `upload` job uses, so a
  `workflow_dispatch` builds the tag and not the dispatched branch tip;
- build the binary (`cargo build --release --locked -p thegn-host --bin thegn`);
- generate bash/zsh/fish/elvish/powershell for both `thegn` and `tg` into a
  staging dir laid out the way a packager wants it
  (`share/bash-completion/completions/thegn`, `share/zsh/site-functions/_thegn`,
  `share/fish/vendor_completions.d/thegn.fish`, and an `elvish/` +
  `powershell/` dir for the two without a convention);
- tar it as `thegn-<tag>-completions.tar.gz`, sha256 it, and
  `gh release upload "<tag>" … --clobber` (`--clobber` for the same re-run
  safety the existing legs rely on — see the CAUTION comment above
  `create-release`);
- add a comment above the job explaining _why_ it is one arch-independent asset
  and not part of the per-target archives: `upload-rust-binary-action`'s
  `include:` only takes files that exist in the repo, and the darwin/musl legs
  cannot always execute their own output.

Set the same scratch-env exports before running the generator in CI.

## Tests / verification

There is no cheap automated gate for this chunk; verify by hand and record the
output in your commit message.

1. `just completions` → the five directories exist and are non-empty for both
   names; `target/completions/zsh/_tg` starts with `#compdef tg` (not `thegn`).
2. `nix build .#default` succeeds, and:
   - `ls result/share/zsh/site-functions` shows `_thegn` and `_tg`;
   - `ls result/share/bash-completion/completions` shows `thegn` and `tg`;
   - `ls result/share/fish/vendor_completions.d` shows `thegn.fish`, `tg.fish`;
   - `head -1 result/share/zsh/site-functions/_tg` names `tg`.
3. `nix build .#dev` succeeds and installs `_thegn-dev` / `_tg-dev`.
4. Sanity-load one script in a real shell:
   `zsh -c 'fpath=(result/share/zsh/site-functions $fpath); autoload -U compinit; compinit; echo ok'`.
5. `just lint` (yamllint + shellcheck cover the workflow and the recipe body).
6. Confirm the build did **not** create a state DB: after `nix build`, no
   `thegn.db` under a real `XDG_STATE_HOME`.

Do not run `just ci` per edit — see the `CLAUDE.md` dev-loop policy. Run
`just lint` and the manual checks above; the pre-push hook is the heavy gate.

## Done criteria

- A Nix install (`nix profile install .#default`, or the HM module) gives a
  user working `thegn` **and** `tg` completions in bash, zsh and fish with no
  rc-file edit and no manual step.
- Cross-compiled builds still succeed (completions omitted, no failure).
- A release carries `thegn-<tag>-completions.tar.gz` plus its checksum.
- `just completions` produces all five shells for both names, into
  `target/completions/`, without touching the real state dir.
- No file outside the three you own is modified.

## Gotchas

- `wrapProgram` after generation, always. Generating from the wrapper works but
  drags the runtime closure into the completion step for no reason.
- `installShellCompletion --cmd <name>` sets the _installed filename_; the
  script's internal command name still comes from `argv[0]` at generation time.
  Both must agree or bash will define a function for one name and register it
  for another.
- The generator buffers before writing because `clap_complete::generate` panics
  on a broken pipe — so `… | head` in a debugging session is safe, but do not
  "simplify" the Nix expression into something that closes the pipe early.
- yamllint runs in `just lint`; match the existing indentation and quoting style
  in `release.yml` or the gate fails on formatting alone.
