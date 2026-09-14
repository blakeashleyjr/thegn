# Select and confirm before snapshotting

THE-595 repairs the merge-queue roadmap (group T, item 758), blocks the THE-586
native retry and follows THE-589/THE-591. Discovery currently commits dirty
worktrees before queue selection, dry-run and confirmation. Make discovery
read-only, then snapshot only admitted local candidates inside the shared CLI/UI
execution path after the pre-fold observation. Missing confirmation must refuse
with a nonzero CLI status.

Add a reusable private native-binary harness covering these authorization edges,
failed gate outcome persistence, no implicit expiry sweep and actual successful
target advancement with retention. It never uses the live queue or database.
