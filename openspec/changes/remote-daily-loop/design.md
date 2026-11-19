# Design

Adopt is a CLI-only `env` subcommand, not a Lead verb. Landing never deletes a
sandbox by default because it holds gitignored state that exists nowhere else.
The reaper runs on the hydrate cadence (300 s throttle) so it only runs while a
client is up, and every destroy goes through `SpritesProvider::destroy` for its
own proof. The pull lock is `$XDG_RUNTIME_DIR/thegn/pull-<sha12>.lock`, 120 s
wait, with the tip re-read after the wait.
