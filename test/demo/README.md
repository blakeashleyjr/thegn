# Scripted terminal demo

## What this is

This is an opt-in scripted session for capturing a repeatable thegn demo. It is
separate from `just e2e`; it adds no muse spec or snapshot baseline.

## Prerequisites

Build the release binary with `just release`, and install `muse` and `git`.
For a GIF, install `agg` from the asciinema project. For a terminal recording,
install `asciinema`.

## Run

Run the capture driver with:

```sh
just demo-script --out ./demo-out
```

The default binary is `target/release/thegn`. Set `THEGN_BIN` to use a different
built binary, or pass `--bin PATH` to the driver. `--size WxH` changes the
terminal dimensions. `--no-prompt` prints manual directions and continues
without waiting for Enter.

The driver creates a throwaway repository and isolated HOME, XDG directories,
git configuration, runtime directory, and private muse socket. The fixture's
initial commit has a fixed author and committer date.

## What you get

- `demo-out/*.txt` contains the named text snapshots from the steps file.
- `demo-out/trace/output.cast` is an asciinema v2 cast from the post-reattach
  launch.
- `demo-out/trace-before-detach/output.cast` is the cast captured before
  detaching.

The private fixture is removed when the driver exits. The output directory is
kept for review and capture.

## Make a GIF

Use `agg` on the pre-detach cast:

```sh
agg demo-out/trace-before-detach/output.cast demo.gif
```

Font size, theme, and playback speed (including `agg`'s `--speed`) are the
owner's capture choices.

## Make a live asciinema recording instead

`asciinema rec -c "just demo-script"` records the driver's own output, not
thegn. To record thegn itself in your terminal, run the steps by hand using
`steps.tsv` as the shot list.

## Manual steps

- Choose the plain shell entry in the new-worktree picker and name the branch
  `demo-feature` if prompted. Muse cannot assert the picker wording because it
  is not a stable documented prompt.
- Accept the `Alt-X` confirmation if it appears. Muse cannot assert the prompt
  wording; verify that `demo-feature` remains with `git branch --list` in the
  fixture repo.
- Narrate the distinction between lowercase `Alt-x` closing a pane and
  uppercase `Alt-X` removing the worktree while keeping its branch. This is
  human narration, so muse cannot assert it as terminal output.

The steps file marks each manual direction. Use `--no-prompt` to print those
directions and continue automatically.

## Troubleshooting

If a step times out, inspect `demo-out/*.txt` and run `muse session logs demo`
while the session is available. The driver prints the isolated
`XDG_RUNTIME_DIR`; use that same directory when
running `thegn daemon stop` to clear a stale daemon.

## Hermetic check

Run `just demo-script-check` to validate the steps, documented key chords, the
driver's shell syntax, and its dry-run mode. The check launches neither thegn
nor muse.

The driver saves local muse cast files; it does not create or publish a GIF or
video. No GIF is committed by this lane.
