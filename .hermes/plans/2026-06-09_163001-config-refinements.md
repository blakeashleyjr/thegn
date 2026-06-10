# Superzej Config Refinements Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Elevate live configuration management in `superzej` with 3 major improvements: resilient safe-reloading that doesn't wipe state on parse errors, visual hot-swapping for deep UI state synchronization, and a generic `--set key=value` CLI override system.

**Architecture:** 
1. `Safe/Atomic Reloading`: Change `Config::load_layered` (or its underlying mechanism) to bubble up `Result<Config, Error>` when parsing TOML, instead of instantly returning default values. The watcher thread handles `Err` by sending a new channel message variant (`Result<Config, String>`) so the event loop can alert the user.
2. `Visual Hot-Swapping`: In the event loop's `config_rx` handler, explicitly map `new_cfg.theme.accent` and `new_cfg.drawer.height` values into `layout::compute`, palette re-theming, and component relayout.
3. `Nested CLI Overrides`: Add `--set <KEY>=<VALUE>` to `Cli` which allows arbitrary dot-path resolution directly into the TOML AST prior to serde parsing, stripping out the massive list of scalar flags in `Cli`.

**Tech Stack:** `toml_edit` (for dot-path AST overrides), `notify`, `tokio` (existing), `clap` (existing).

---

## Task 1: Refactor `load_layered` for Safe Parsing

**Objective:** Ensure a broken TOML save doesn't erase configuration silently, but returns a result instead.

**Files:**
- Modify: `crates/superzej-core/src/config.rs:952`
- Modify: `crates/superzej-host/src/run.rs:645`

**Step 1: Add `try_load_layered` method**

In `crates/superzej-core/src/config.rs`, change the parser behavior and add a fallback option. 
```rust
impl Config {
    pub fn try_load_layered(
        env: &dyn EnvSource,
        flags: Option<ConfigOverlay>,
        path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let file = path.unwrap_or_else(Self::path);
        let mut cfg: Config = match std::fs::read_to_string(&file) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("parse error: {e}"))?,
            Err(_) => Config::default(),
        };
        env_overlay(env).apply(&mut cfg);
        if let Some(f) = flags {
            f.apply(&mut cfg);
        }
        cfg.post_process();
        Ok(cfg)
    }
}
```

Change `load_layered` to call `try_load_layered` and fallback to default (so we don't break existing usages where error swallowing is expected on startup).
```rust
    pub fn load_layered(
        env: &dyn EnvSource,
        flags: Option<ConfigOverlay>,
        path: Option<PathBuf>,
    ) -> Self {
        match Self::try_load_layered(env, flags, path) {
            Ok(cfg) => cfg,
            Err(e) => {
                config_warn(&e);
                Config::default()
            }
        }
    }
```

**Step 2: Update host channel payload**

In `crates/superzej-host/src/run.rs`, change `config_tx` to send `Result<Config, String>`.

```rust
    // Line 624
    let (config_tx, config_rx) = std::sync::mpsc::channel::<Result<superzej_core::config::Config, String>>();

    // Line 641
    let new_cfg_res = superzej_core::config::Config::try_load_layered(
        &superzej_core::config::ProcessEnv, 
        Some(overlay_clone.clone()), 
        config_clone.clone()
    );
    let _ = config_tx.send(new_cfg_res);
```

**Step 3: Handle in event loop**

In `crates/superzej-host/src/run.rs:1003`:
```rust
        while let Ok(cfg_res) = config_rx.try_recv() {
            match cfg_res {
                Ok(new_cfg) => {
                    keymap = crate::keymap::default_keymap_with_config(&new_cfg);
                    model.status = "Config reloaded".into();
                    // We'll add visual hot-swapping here later
                }
                Err(e) => {
                    model.status = format!("Config error: {}", e).into();
                }
            }
            dirty = true;
        }
```

**Step 4: Verify build**
Run: `cargo build -p superzej-host`
Expected: SUCCESS

---

## Task 2: Implement Deep UI State Synchronization

**Objective:** Map live configuration updates deeply into the visual components (drawer size, accents) so that they apply in real-time.

**Files:**
- Modify: `crates/superzej-host/src/run.rs:1003`

**Step 1: Save `config` inside `run.rs` event loop**

Currently the event loop discards the `Config` block after using it to hydrate `keymap`, but relies on static limits later in the code. Let's keep `current_config` as a mutable loop state.
Above `loop {` (around line 865), add:
```rust
    let mut current_config = keymap.config().clone();
```

**Step 2: Update hot-swapping behavior**

In the event loop `while let Ok(cfg_res) = config_rx.try_recv()`:
```rust
                Ok(new_cfg) => {
                    keymap = crate::keymap::default_keymap_with_config(&new_cfg);
                    current_config = new_cfg;
                    model.status = "Config reloaded".into();
                    need_relayout = true; // Force relayout for drawer/chrome changes
                }
```

**Step 3: Link `current_config` to Drawer Height**

In `crates/superzej-host/src/run.rs` inside the `if dirty` block (around line 1050), change the hardcoded height:
```rust
                    let height = current_config.drawer.height.min(rows as u32) as usize; 
                    // Make sure Config defines drawer.height as u32 or usize.
```

**Step 4: Verify build**
Run: `cargo build -p superzej-host`
Expected: SUCCESS

---

## Task 3: Arbitrary Nested CLI Overrides

**Objective:** Migrate away from 20 flat CLI struct flags and implement a generic `--set key=value` parser that intercepts the base config file *before* Serde validation.

**Files:**
- Modify: `crates/superzej-cli/src/cli.rs`
- Modify: `crates/superzej-host/src/main.rs`
- Modify: `crates/superzej-core/src/config.rs`

**Step 1: Simplify CLI Struct**

In `crates/superzej-cli/src/cli.rs` and `crates/superzej-host/src/main.rs`:
```rust
pub struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,
    
    /// Override a config value (e.g. `--set theme.accent=cyan --set drawer.height=15`)
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
    
    #[command(subcommand)]
    pub command: Option<Command>,
}
```
*Note: Drop all the individual scalar options.*

**Step 2: Update `to_overlay`**

Remove the `to_overlay` helper, as we will handle overrides more organically. We will replace the `ConfigOverlay` usage inside `load_layered` with an AST mutation step.

**Step 3: Modify `load_layered`**

In `crates/superzej-core/src/config.rs`, alter `try_load_layered`:

```rust
    pub fn try_load_layered(
        env: &dyn EnvSource,
        cli_overrides: &[String], // <-- Pass CLI generic overrides here
        path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let file = path.unwrap_or_else(Self::path);
        
        let file_contents = std::fs::read_to_string(&file).unwrap_or_else(|_| "".to_string());
        
        // Parse into `toml_edit::DocumentMut` instead of direct `toml::from_str`
        let mut doc = file_contents.parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse error: {e}"))?;
            
        // Apply dot-notation overrides directly to AST
        for ov in cli_overrides {
            if let Some((key, val)) = ov.split_once('=') {
                // Extremely simple dot-notation logic. 
                // Alternatively, convert Document to a toml::Value tree and manipulate.
                // Let's use `toml::Value` for easier manipulation
            }
        }
        
        let mut cfg: Config = toml_edit::de::from_document(doc).map_err(|e| format!("parse error: {e}"))?;
        
        env_overlay(env).apply(&mut cfg);
        cfg.post_process();
        Ok(cfg)
    }
```
*Note for implementer: Applying dot-notation to a `toml_edit` AST or a `serde_json::Value` (by serializing back and forth) is the core technical challenge here. Pick the simplest path available in the existing workspace crates.*

**Step 4: Execute Handoff**
Since this task introduces breaking changes to CLI parsing and `Config` architecture, run all regression tests `just test` after implementation.

---

## Execution Handoff

Plan complete and saved. Ready to execute using subagent-driven-development — I'll dispatch a fresh subagent per task with two-stage review (spec compliance then code quality). Shall I proceed?