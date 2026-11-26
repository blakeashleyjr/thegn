## Licence handling for dual-licensed and copyleft crates

- Where a crate offers a choice (for example `r-efi` is `MIT OR Apache-2.0 OR LGPL-2.1-or-later` and `self_cell` is `Apache-2.0 OR GPL-2.0-only`), thegn takes it under the permissive option (MIT or Apache-2.0). No crate in the list requires thegn itself to be copyleft.
- The three MPL-2.0 crates (`colored`, `option-ext`, `uluru`) are used unmodified. MPL-2.0 is file-level copyleft; their complete source is published on crates.io at the exact versions above, which satisfies the source-availability obligation.
- `ring` is `Apache-2.0 AND ISC` and carries OpenSSL-licensed code; `deny.toml` clarifies it as `MIT AND ISC AND OpenSSL`.
- `inferno` (`CDDL-1.0`, file-level copyleft) comes only through `pprof`, the optional `profiling` feature of the host crate. It is not in release builds and is outside the `deny.toml` graph; if a profiling build were ever distributed, `inferno`'s source is on crates.io at the version above.
- `webpki-roots` is `CDLA-Permissive-2.0` (Mozilla CA root data); `terminfo` is `WTFPL`; `notify` is `CC0-1.0`.

## Vendored source

- `vendor/termwiz` (termwiz 0.23.3 with a destructor-only patch, see `vendor/termwiz/PATCH.md`): MIT, Copyright (c) 2018 Wez Furlong, from <https://github.com/wezterm/wezterm>. The licence is kept in `vendor/termwiz/LICENSE.md`. Obligation: keep the copyright and permission notice.
- `vendor/termwiz/data/emoji-data.txt` and `vendor/termwiz/data/emoji-variation-sequences.txt`: Unicode data files, Copyright Unicode, Inc., distributed under the Unicode Terms of Use (now the permissive Unicode licence). Obligation: keep the copyright header in the files.
- `vendor/termwiz/data/wezterm.terminfo`, `vendor/termwiz/data/xterm-256color-italic.terminfo` and the compiled `wezterm` and `xterm-256color` entries: terminfo descriptions shipped with termwiz, covered by termwiz's MIT licence. The `xterm-256color` entry follows the ncurses and xterm terminfo database, which is MIT-style licensed.
- `config/yazi/plugins/git.yazi/main.lua`: vendored verbatim from yazi-rs/plugins, MIT, Copyright (c) 2023 yazi-rs. Obligation: keep the copyright and permission notice (the file header names it; the MIT text is in the Apache and MIT notices below). `tg-drawer-close.yazi` and `tg-drawer-editor.yazi` are thegn's own.

## Native libraries compiled into the binary

- SQLite (bundled by `libsqlite3-sys`, via `rusqlite`): public domain. No obligation.
- libgit2 1.9.7 (vendored by `libgit2-sys`, pulled in by the file-search engine in the host crate): GPL-2.0 with the libgit2 linking exception, which grants unlimited permission to link the compiled library into other programs and distribute the result without restriction. It also bundles xdiff (LGPL-2.1-or-later), PCRE2 (BSD), zlib, llhttp (MIT) and ntlmclient. Obligation: include its copyright and licence text (shipped in the `libgit2-sys` source on crates.io) and do not modify libgit2 files without publishing the changes. The Rust binding crate itself is `MIT OR Apache-2.0`.
- LMDB (bundled by `lmdb-master-sys`, via `heed`): OpenLDAP Public License 2.8, Copyright 2011-2021 Howard Chu, Symas Corp. The crate metadata says `Apache-2.0`, which describes only the Rust binding. Obligation: binary redistributions must reproduce the copyright notice and licence text below.
- whisper.cpp and ggml (bundled by `whisper-rs-sys`, only in builds with `voice-native` or `voice-metal`): MIT, Copyright (c) 2023-2024 The ggml authors. The binding crate is `Unlicense` (`whisper-rs` is `Unlicense` too). Obligation: keep the MIT notice when such a build is distributed.
- `ring`: see above.

## Runtime-fetched models and tools (not shipped)

- Whisper GGML model files (`ggml-*.bin`): downloaded on request from <https://huggingface.co/ggerganov/whisper.cpp> at a pinned revision, verified by size and SHA-256. The Whisper model weights are released by OpenAI under the MIT licence. thegn does not redistribute them.
- `difft` (difftastic, MIT): resolved from a user override, `PATH` or an already-installed copy. thegn ships no copy and has no download pin.
- `bugstalker` (installed from crates.io on request) and user-chosen tools such as `lazygit`, `yazi` and `gum`: separate programs under their own licences, started as child processes.
- Syntax highlighting data inside `syntect` (`default-syntaxes`, `default-themes`): the Sublime Text package syntaxes and the base16 themes bundled by the crate, under the licences recorded in the syntect repository.

## Nix and release packaging

The Nix package and release tarball put these separate executables on `PATH` for thegn to run. They are not linked into thegn (mere aggregation) and keep their own licences; their sources are available from nixpkgs:

- `git` (GPL-2.0-only), `coreutils` (GPL-3.0-or-later), `bubblewrap` (LGPL-2.0-or-later) and `passt` (GPL-2.0-or-later and BSD-3-Clause) on Linux.
- `fzf`, `gum`, `lazygit`, `yazi`, `delta`, `gh` and `gdu`: MIT.
- The optional `thegn-batteries` wrapper adds `alacritty` (Apache-2.0) and the FiraCode Nerd Font (SIL OFL-1.1, with MIT-licensed patcher components). Obligation: the font's OFL text travels with the font in its Nix store path.

The licences in this section for Nix-provided tools come from nixpkgs metadata as pinned in `flake.lock`; re-check them when the lock is bumped.

## Artwork, themes and fonts

- `config/thegn.svg` and `docs/assets/*.png` are thegn's own artwork.
- thegn bundles no font files. The `prism`, `storm`, `light`, `abyss`, `ember` and `aurora` themes are original. The remaining theme presets are colour palettes named after community themes (Catppuccin, Nord, Dracula, Gruvbox, Tokyo Night, Solarized, Rose Pine, One Dark, Monokai Pro, Ayu, Everforest, Kanagawa, Night Owl). Only colour values are used, no theme source files; the upstream palettes are MIT-licensed except Monokai Pro, which is a commercial theme, so its preset needs an owner decision.
- No sound files, icon fonts, JavaScript or TypeScript packages are bundled.

## NOTICE texts

Apache-2.0 crates in the lock file that ship a `NOTICE` file:

- `moka` 0.12.16 (`MIT OR Apache-2.0`):

```text
Additional Notices for Moka

The majority of the Moka library is dual-licensed under the MIT and Apache 2.0
licenses.

However, the following files are an exception and are licensed solely under
the Apache License 2.0:

- src/common/frequency_sketch.rs
- src/common/timer_wheel.rs

These files were ported from the Java Caffeine library and are not dual-licensed.

Please refer to the LICENSE-APACHE file for more details on the Apache License 2.0.
```

Third-party notice carried by `cfg_aliases` 0.1.1 and 0.2.2 (`NOTICES.md`):

```text
The `cfg_aliases!` macro uses a lot of the code from the
`tectonic_cfg_support::target_cfg!` macro, which is under the MIT License.
Copyright (c) the Tectonic Project contributors.
```

OpenLDAP Public License 2.8 notice for LMDB:

```text
Copyright 2011-2021 Howard Chu, Symas Corp. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted only as authorized by the OpenLDAP Public License
(version 2.8, 17 August 2003), a copy of which is distributed with the
lmdb-master-sys source at lmdb/libraries/liblmdb/LICENSE.
```

MIT notice for whisper.cpp and ggml, and for the vendored yazi git plugin:

```text
whisper.cpp / ggml: Copyright (c) 2023-2024 The ggml authors
yazi-rs/plugins git.yazi: Copyright (c) 2023 yazi-rs
termwiz: Copyright (c) 2018 Wez Furlong

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

The full Apache-2.0 text is in `LICENSE-APACHE`; the full text of every other licence ships in the corresponding crate's source on crates.io.
