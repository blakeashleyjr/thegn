# Live upgrades

The in-place installer (`just live` replacing the installed binary and
relinking launchers) was removed in 0.1.0-alpha.10x.

- **Stable** is installed by Nix/home-manager from a published release. See the
  README's Install section, and `RELEASING.md` for the bump command.
- **`just live`** builds and runs the dev instance at `~/.thegn-devel/bin/thegn`
  and never touches stable. See `thegn help two-instances`.
- Old `live-backup-*` recovery directories in the stable state dir are left in
  place. Delete them yourself when you no longer need them.
