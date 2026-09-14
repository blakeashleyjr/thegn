# Fixture ownership and deterministic inputs

The watchdog Harness owns an existing temporary worktree and all state through
pane cleanup. It disables sandbox, placement and daemon paths and uses a scoped,
rc-free shell adapter around the unchanged production clean-shell command.
Platform-specific fixture code lives under the platform seam; no-spawn watchdog
cases remain portable. The actual POSIX spawn case is selected on Unix.

Git fixtures create their bare remote with an explicit main HEAD, supply private
author/committer identities to the deliberately conflicting merge, and assert
conflict exit status before inspecting MERGE_HEAD. TempDir guards own both
fixtures. Developer global defaults are not restored to hide missing setup.

The failed watchdog run left one private rootless overlay tree. Inspection found
no mount or running container helper for it, and cleanup removed only that exact
owned tree in a separately isolated user namespace. No live container state or
broad cleanup operation was used.
