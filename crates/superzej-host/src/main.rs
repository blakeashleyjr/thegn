//! superzej-host — the native terminal compositor.
//!
//! Phase 1 (the GO/NO-GO spike): a tokio event loop driving a single
//! portable-pty pane through a `PaneEmulator` grid, composited into a termwiz
//! `Surface` that diff-flushes to the outer terminal (the "no-flash" mechanism).

mod center;
mod chrome;
mod compositor;
mod copymode;
mod emulator;
mod keymap;
mod layout;
mod palette;
mod pane;
mod run;
mod sequence;
mod session;

fn main() -> anyhow::Result<()> {
    run::main()
}
