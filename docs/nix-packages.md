# Nix package topology

The flake exposes stable packages for ordinary use and a dev-channel package
for experimental provider and remote features. The bridge posture below is the
one currently built by each attribute.

| Attribute          | What it is                                                                            | Provider bridge                                            |
| ------------------ | ------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `.#default`        | Stable host package and default profile install.                                      | On x86_64-linux, an adjacent `thegn-musl`; elsewhere none. |
| `.#thegn`          | Alias of the default stable package.                                                  | On x86_64-linux, an adjacent `thegn-musl`; elsewhere none. |
| `.#thegn-nobridge` | Stable host package without the adjacent bridge; used by the routine Nix build check. | no bridge                                                  |
| `.#batteries`      | Stable host with the pinned terminal and font setup.                                  | no bridge                                                  |
| `.#dev`            | Experimental dev-channel host, installed as `thegn-dev`.                              | On x86_64-linux, `THEGN_BRIDGE_BINARY`; elsewhere none.    |
| `.#thegn-dev`      | Alias of the dev-channel host package.                                                | On x86_64-linux, `THEGN_BRIDGE_BINARY`; elsewhere none.    |
| `.#thegn-musl`     | Static musl bridge binary for provider microVMs.                                      | the bridge itself                                          |

## Run without installing

`nix run github:blakeashleyjr/thegn` builds the stable host and starts it
straight from the flake, with nothing added to a Nix profile. The bare
reference resolves `apps.default`, which names the same `bin/thegn` wrapper
`.#default` installs, so the pinned yazi and the runtime tools are on `PATH`
exactly as they are for an installed package.

```sh
nix run github:blakeashleyjr/thegn        # stable host, one command
nix run github:blakeashleyjr/thegn#dev    # dev channel, as thegn-dev
nix run github:blakeashleyjr/thegn/v<tag> # pin a published release tag
```

The first run compiles the workspace and its dependencies — and on x86_64
Linux also the musl bridge that `.#default` carries — so it is a long build, not
a download. Later runs reuse the store paths, so the only files that change are
the ones thegn itself keeps in its state directory.

## Bridge discovery

Release archives, Homebrew and the deb/rpm/AUR packages ship the same bridge as
`thegn-musl` beside `thegn` (x86_64 Linux musl; Sprites and Vercel sandboxes are
x86_64 Linux, including when the host is a Mac).

The host checks `THEGN_BRIDGE_BINARY` first, then looks for an adjacent
`thegn-musl` next to the running executable. If neither is available, it runs
without a resident bridge. `thegn doctor` reports `artifact none` and says
provider connects run without a resident bridge, with a hint to set
`THEGN_BRIDGE_BINARY` or install `thegn-musl` beside `thegn`.

The dev package uses the explicit environment-variable reference to the Nix
store path of the bridge. The default package uses the adjacent binary. These
can share a profile without colliding at `bin/thegn-musl`.

## Build cost

On x86_64-linux, `.#default` and `.#dev` each pull in the musl cross-compile of
the workspace, about double the cost of a plain stable build. `.#thegn-nobridge`
avoids that compile and is what CI builds routinely. These are qualitative
build-cost descriptions; no timing or closure-size measurements are claimed.

The same applies to `nix run github:blakeashleyjr/thegn`: the default app is
`.#default`, so the first bare run pays for the native build plus the musl
cross-build.

## Module defaults and stable users

The Home Manager and nix-darwin modules default `programs.thegn.package` to
`.#default`. Stable-channel builds clamp provider and remote features off by
default, so the bridge included beside the stable default package is inert for
stable users. Observe and plugin app tabs are available in stable; provider and
remote features remain experimental and dev-channel only.

## Platform limits

Linux x86_64 is the supported platform for the public alpha; macOS is
best-effort. The bridge build is available only for x86_64-linux. Other
non-x86_64-linux systems have no bridge build, and `.#dev` on those systems has
no `THEGN_BRIDGE_BINARY` reference. Windows is deferred.
