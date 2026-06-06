#!/usr/bin/env bash
# test/gen-fixture.sh — build a heavy, realistic, FULLY ISOLATED superzej
# instance for stress-testing the sidebar/tabbar/dashboard and the `workspaces`
# perf fix.
#
# Creates REPOS repos (default 20), each with a random commit history and 3-20
# git worktrees in random ahead/behind/dirty states, under its own instance root
# (~/.superzej-NAME) — own DB, config, worktrees, cache, socket. It NEVER touches
# your daily-driver superzej (matches the `just start-term` / smoke.sh sandboxing
# rules). Idempotent: it wipes & rebuilds the instance root each run.
#
# It also writes layout-stress.kdl: a session layout that pre-opens a slice of
# worktrees as real tabs, so the sidebar tree + tabbar are stressed at launch.
# `just stress NAME` launches the dev-tui against it.
#
# Usage: test/gen-fixture.sh [NAME] [REPOS] [OPEN_TABS]
#   NAME       instance suffix   (default: stress)  -> ~/.superzej-NAME
#   REPOS      number of repos   (default: 20)
#   OPEN_TABS  worktree tabs the generated layout pre-opens (default: 100)
# Env: SZ=path/to/superzej (default ./target/debug/superzej), SEED=int (default 1234).
set -euo pipefail

NAME="${1:-stress}"
REPOS="${2:-20}"
OPEN="${3:-100}"
SEED="${SEED:-1234}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SZ="${SZ:-$ROOT_DIR/target/debug/superzej}"
SZ="$(cd "$(dirname "$SZ")" && pwd)/$(basename "$SZ")"
[[ -x $SZ ]] || {
  echo "not executable: $SZ (run: cargo build)" >&2
  exit 1
}

INST="$HOME/.superzej-$NAME"
REPO_DIR="$INST/fixtures/repos"
WT_DIR="$INST/worktrees"

# Full isolation — own state/config/cache/socket under the instance root. The
# same env is reused by test/perf.sh and the `just stress` launch so the DB,
# worktrees and config all line up.
export SUPERZEJ_DIR="$INST"
export XDG_STATE_HOME="$INST/state"
export XDG_CONFIG_HOME="$INST/config"
export XDG_CACHE_HOME="$INST/cache"
export ZELLIJ_SOCKET_DIR="$INST/run"
unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID
export SUPERZEJ_NO_EXEC=1 # register repos without cold-starting a session
export GIT_AUTHOR_NAME=fixture GIT_AUTHOR_EMAIL=fixture@local
export GIT_COMMITTER_NAME=fixture GIT_COMMITTER_EMAIL=fixture@local

echo "==> rebuilding instance: $INST  (repos=$REPOS open_tabs=$OPEN seed=$SEED)"
rm -rf "$INST"
mkdir -p "$REPO_DIR" "$WT_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME/superzej"

cat >"$XDG_CONFIG_HOME/superzej/config.toml" <<EOF
worktrees_dir = "$WT_DIR"
workspaces_dir = "$REPO_DIR"
repo_roots = ["$REPO_DIR"]
name_scheme = "numbered"
base_branch = "auto"
EOF

# --- deterministic pseudo-randomness -------------------------------------
RANDOM="$SEED"
rnd() { echo $(($1 + RANDOM % ($2 - $1 + 1))); } # rnd MIN MAX -> [MIN,MAX]
chance() { ((RANDOM % 100 < $1)); }              # chance PCT -> true PCT% of the time

commit_in() { # commit_in DIR MSG  — a commit touching a random file
  local d="$1" msg="$2" f
  f="$d/src_$((RANDOM)).txt"
  echo "$RANDOM-$RANDOM" >"$f"
  git -C "$d" add -A
  git -C "$d" commit -q -m "$msg"
}

dirty_up() { # leave an uncommitted mess in a worktree
  local d="$1"
  # modify a tracked file
  local tracked
  tracked="$(git -C "$d" ls-files | head -1)"
  [[ -n $tracked ]] && echo "wip $RANDOM" >>"$d/$tracked"
  # stray untracked files
  local n
  n="$(rnd 1 3)"
  while ((n-- > 0)); do echo scratch >"$d/untracked_$((RANDOM)).tmp"; done
}

# Repo names: a pool, plus two deliberate same-basename repos under different
# parents ("washu") so the sidebar/slug disambiguation path gets exercised.
WORDS=(orbit nimbus quartz vellum cobalt cinder harbor zephyr lattice umbra
  pivot saffron tundra dapple gossamer fathom relay marrow thistle vesper)

repo_dirs=()
for ((i = 0; i < REPOS; i++)); do
  case "$i" in
  0) d="$REPO_DIR/east/washu" ;; # duplicate basename A
  1) d="$REPO_DIR/west/washu" ;; # duplicate basename B
  *) d="$REPO_DIR/${WORDS[i % ${#WORDS[@]}]}-$i" ;;
  esac
  repo_dirs+=("$d")
done

total_wt=0
for d in "${repo_dirs[@]}"; do
  mkdir -p "$d"
  git init -q -b main "$d"
  commit_in "$d" "init"
  # Base history: a random run of commits on main.
  hist="$(rnd 3 25)"
  for ((c = 0; c < hist; c++)); do commit_in "$d" "main commit $c"; done

  name="$(basename "$d")"
  wt_count="$(rnd 3 20)"
  for ((w = 0; w < wt_count; w++)); do
    branch="sz/feat-$w"
    wt="$WT_DIR/$name/feat-$w"
    # Same-basename repos would collide under $WT_DIR/<name>; disambiguate.
    [[ -e $wt ]] && wt="$WT_DIR/$name-$RANDOM/feat-$w"
    mkdir -p "$(dirname "$wt")"
    git -C "$d" worktree add -q -b "$branch" "$wt" main 2>/dev/null || continue
    total_wt=$((total_wt + 1))

    # ahead: extra commits on the worktree branch
    ahead="$(rnd 0 6)"
    for ((a = 0; a < ahead; a++)); do commit_in "$wt" "feat $w ahead $a"; done
    # behind: advance main so this worktree trails it
    if chance 45; then
      behind="$(rnd 1 4)"
      for ((b = 0; b < behind; b++)); do commit_in "$d" "main moves $b"; done
    fi
    # dirty working tree
    chance 60 && dirty_up "$wt"
  done

  # Register the repo so it shows in the sidebar's `workspaces` inventory.
  "$SZ" new-workspace "$d" >/dev/null
done

echo "==> generated $REPOS repos, $total_wt worktrees"

# --- generate layout-stress.kdl ------------------------------------------
# One tab per worktree (capped at OPEN), each wrapped in the same chrome as
# layouts/superzej.kdl so the sidebar/tabbar/panel/statusbar render per tab.
# Tab names use superzej's own `{slug}/{branch_label}` so the sidebar groups
# them under their repo. The center pane is a plain shell at the worktree cwd.
LAYOUT="$INST/layout-stress.kdl"
PLUG="file:~/.local/share/superzej"

# Snapshot the inventory to a file first — piping `superzej worktrees` straight
# into `head` would SIGPIPE the writer (superzej panics on a closed stdout).
WT_TSV="$INST/worktrees.tsv"
"$SZ" worktrees >"$WT_TSV"

emit_chrome() { # emit_chrome CWD  -> a chrome-wrapped tab body with a shell at CWD
  # Multi-line node-per-line form — zellij's KDL parser rejects inline
  # `{ plugin … }` bodies, so this mirrors layouts/superzej.kdl exactly.
  cat <<EOF
        pane split_direction="horizontal" {
            pane size=1 borderless=true {
                plugin location="$PLUG/tabbar.wasm"
            }
            pane split_direction="vertical" {
                pane size="12%" name="" {
                    plugin location="$PLUG/sidebar.wasm"
                }
                pane focus=true cwd="$1"
                pane size="27%" name="" {
                    plugin location="$PLUG/panel.wasm"
                }
            }
            pane size=1 borderless=true {
                plugin location="$PLUG/statusbar.wasm"
            }
        }
EOF
}

{
  echo "// GENERATED by test/gen-fixture.sh — heavy stress layout ($OPEN worktree tabs)."
  echo "layout {"
  echo '    tab name="home" focus=true {'
  emit_chrome "$REPO_DIR"
  echo "    }"
  # Worktree tabs from superzej's own inventory (slug<TAB>label<TAB>path).
  head -n "$OPEN" "$WT_TSV" | while IFS=$'\t' read -r slug label path; do
    [[ -n $slug && -d $path ]] || continue
    printf '    tab name="%s/%s" {\n' "$slug" "$label"
    emit_chrome "$path"
    echo "    }"
  done
  # Swap layouts (Alt+[ / Alt+]) — mirror layouts/superzej.kdl.
  cat <<EOF
    tab_template name="chrome" {
        pane split_direction="horizontal" {
            pane size=1 borderless=true {
                plugin location="$PLUG/tabbar.wasm"
            }
            pane split_direction="vertical" {
                pane size="12%" name="" {
                    plugin location="$PLUG/sidebar.wasm"
                }
                children
                pane size="27%" name="" {
                    plugin location="$PLUG/panel.wasm"
                }
            }
            pane size=1 borderless=true {
                plugin location="$PLUG/statusbar.wasm"
            }
        }
    }
    swap_tiled_layout name="vertical" {
        chrome min_panes=6 {
            pane split_direction="horizontal" {
                children
            }
        }
    }
    swap_tiled_layout name="horizontal" {
        chrome min_panes=6 {
            pane split_direction="vertical" {
                children
            }
        }
    }
    swap_tiled_layout name="stacked" {
        chrome min_panes=6 {
            pane stacked=true {
                children
            }
        }
    }
}
EOF
} >"$LAYOUT"

opened="$(head -n "$OPEN" "$WT_TSV" | grep -c .)"
echo "==> wrote $LAYOUT ($opened worktree tabs + home)"
echo "==> launch:  just stress $NAME      perf:  just perf $NAME"
