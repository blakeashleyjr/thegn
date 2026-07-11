# Plan: Verify and restore font reloading with selected monospace fonts

**Goal:** Answer the user's questions: "did our font reloading survive the redesign?" and "Should have these: [list of 10 specific fonts]". The redesign (which stripped out the old `thegn` UI and rebuilt it via `termwiz`) must still support the `just font name="..."` realtime alacritty reloading feature. I will also ensure the Nix flake / development environment correctly bundles the 10 requested fonts.

**Architecture:**
The font reloading feature currently works by updating the `config/alacritty.toml` file in-place using `sed` within the `just font name="..."` task. Alacritty is configured to watch this file and live-reload the font without restarting. The requested fonts need to be added to the Nix dependencies to ensure they are available in the development environment and CI/CD pipelines.

**Tech Stack:** `nix`, `just`, `sed`, `alacritty`

---

## Task 1: Verify Font Reloading Mechanism

**Objective:** Confirm that `just font name="..."` still modifies `config/alacritty.toml` and that Alacritty correctly picks up the change without restarting.

**Step 1: Check the `justfile`**
Review the `justfile` to ensure the `font` target exists and modifies `config/alacritty.toml` as expected.

**Step 2: Test the modification**
Run `just font name="Hack Nerd Font"` and verify `config/alacritty.toml` reflects the change.

**Step 3: Confirm Alacritty live-reload**
Ensure Alacritty's default behavior (which watches its config file) is not disabled or bypassed by the redesign.

---

## Task 2: Add Requested Fonts to Nix Environment

**Objective:** Add the 10 specific fonts requested by the user to the `flake.nix` and `devenv.nix` files so they are available in the `nix develop` / `devenv shell` environments.

**Requested Fonts:**

1. Victor Mono (`nerd-fonts.victor-mono`)
2. JetBrains Mono (`nerd-fonts.jetbrains-mono`)
3. Cascadia Code (`nerd-fonts.caskaydia-cove`)
4. Source Code Pro (`nerd-fonts.sauce-code-pro`)
5. Monoid (`nerd-fonts.monoid`)
6. Nerd Fonts (General / Awesome Fonts) (`font-awesome`)
7. Iosevka (`nerd-fonts.iosevka`)
8. Inconsolata (`nerd-fonts.inconsolata`)
9. Hack (`nerd-fonts.hack`)
10. Fira Code (`nerd-fonts.fira-code`)

**Files:**

- Modify: `flake.nix` (or wherever fonts are declared for the dev shell)
- Modify: `devenv.nix` (if applicable)

**Step 1: Add fonts to `flake.nix` / `devenv.nix`**
Find the `packages` list in `devenv.nix` and add the corresponding font packages from Nixpkgs.

**Step 2: Verify Nix environment**
Run `nix develop` or `devenv shell` to ensure the fonts are downloaded and available via `fc-list`.

---

## Task 3: Update `just fonts` to Include New Fonts

**Objective:** Ensure the `just fonts` command accurately lists the newly added fonts, making it easy for the user to select them.

**Files:**

- Modify: `justfile`

**Step 1: Review `just fonts` logic**
The current logic in `justfile` is:
`@fc-list : family | tr ',' '\n' | grep -i 'nerd font' | grep -iv 'mono\b.*propo\|propo' | sort -u`

**Step 2: Ensure compatibility**
Check if the newly added fonts (like `monoid` or `iosevka`) are picked up by this `grep`. If not, adjust the grep pattern to include them, or ensure the Nerd Font versions of these fonts are installed.

---

## Task 4: Final Validation

**Objective:** Perform an end-to-end test of the font switching workflow.

**Step 1:** Run `just fonts` to list all available fonts.
**Step 2:** Choose a font, e.g., `just font name="Iosevka Nerd Font"`.
**Step 3:** Verify `config/alacritty.toml` is updated.
**Step 4:** Revert back to the default `FiraCode Nerd Font`.
