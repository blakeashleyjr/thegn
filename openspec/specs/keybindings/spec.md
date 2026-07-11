# Keybindings

## Purpose

Every thegn action is reachable through a rebindable keymap registry that
supports modal keymaps, a leader/prefix layer, and chorded sequences. Bindings
layer from presets through user, profile, and workspace overrides; conflicts are
detected at load; and pending sequences surface which-key hints.

## Requirements

### Requirement: All actions are rebindable through the keymap registry

Every bound action SHALL be rebindable through the keymap registry, which MUST support modal keymaps, a leader/prefix layer, and chorded/sequence bindings.

#### Scenario: Rebind an action

- **WHEN** the user binds a key to an action in config
- **THEN** that key invokes the action

#### Scenario: Chord sequence fires

- **WHEN** the user types a configured multi-key sequence
- **THEN** the sequence matcher resolves it to its action

### Requirement: Conflicts are detected at load

The keymap SHALL detect binding conflicts when configuration is loaded.

#### Scenario: Two bindings collide

- **WHEN** configuration binds the same key to two actions
- **THEN** the conflict is detected at load

### Requirement: Bindings layer from presets through workspace overrides

Bindings SHALL layer in order — preset (IDE/vim/emacs) → user `[keybinds]` → per-profile → per-workspace — so that more specific layers win, and a first-launch picker MAY choose a preset that persists.

#### Scenario: Per-workspace override wins

- **WHEN** a workspace defines a keybind that differs from the user default
- **THEN** the workspace binding takes effect in that workspace

#### Scenario: Which-key hint for a pending sequence

- **WHEN** a partial sequence has been typed
- **THEN** the possible continuations are surfaced as which-key hints
