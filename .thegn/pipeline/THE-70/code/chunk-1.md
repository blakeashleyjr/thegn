# THE-70 chunk 1 — keyboard-reporting probe: parsing + policy (pure, `thegn-core`)

Read `.thegn/pipeline/THE-70/architect/design.md` §1.4, §2 (D3/D4), §3, §4
before starting. This chunk is **pure logic only** — no I/O, no wiring, no
callers. Chunk 3 consumes what you build.

## Files touched (exact, exhaustive)

- `crates/thegn-core/src/termcaps.rs` — the only file. Production code **and**
  its `#[cfg(test)] mod tests` at the bottom of the same file.

Do not touch anything else. If you believe you need another file, stop and say
so in the commit body rather than widening the diff.

## Overlap / dependency

- **Parallel-safe with chunk 2** (different crate, no shared file, no API
  dependency in either direction).
- **Chunk 3 depends on this chunk.** Keep the public API exactly as specified
  below so chunk 3 compiles against it without renegotiation.
- Deliberately **not** touching `TermCaps`: it has a struct literal at
  `crates/thegn-host/src/cmd/doctor.rs:3002` that a new field would break,
  which would couple this chunk to the host. `ProbeResult` derives `Default`
  and is only ever built via `ProbeResult::default()` / `interpret_probe`, so
  adding fields there breaks nothing.

## Approach

### 1. Extend `ProbeResult` (`termcaps.rs:886-894`)

Add two fields, both `Option`, both defaulting to `None` ("the terminal did not
answer that query"):

```rust
/// xterm `modifyOtherKeys` level reported by XTQMODKEYS
/// (`CSI > 4 ; <Pv> m`). `None` = the terminal did not answer the query.
pub modify_other_keys: Option<u8>,
/// kitty keyboard-protocol flags reported by the progressive-enhancement
/// query (`CSI ? <flags> u`). `None` = the terminal did not answer.
pub kitty_keyboard: Option<u8>,
```

Keep `#[derive(Default)]` working (both are `Option`, so it does).

### 2. Extend `interpret_probe` (`termcaps.rs:900-927`)

Parse the two replies out of the same raw byte buffer, alongside the existing DA
and XTVERSION handling. Seeing either reply also implies `responded = true`.

- **XTQMODKEYS reply:** `ESC [ > 4 ; <Pv> m`. Match the literal `\u{1b}[>4;`
  prefix, then parse the ASCII digits up to the terminating `m`. Store `Pv` in
  `modify_other_keys`. A malformed/overflowing value ⇒ leave `None`.
- **kitty reply:** `ESC [ ? <flags> u`. Match `\u{1b}[?`, then digits, then `u`.
  Store the flags in `kitty_keyboard`. Note `\u{1b}[?` is also the DA prefix —
  disambiguate on the **terminator**: `u` for kitty, `c` for DA. The DA reply
  contains `;` and ends in `c`; do not let it satisfy the kitty match, and do
  not let a kitty reply satisfy the DA match.
- Search the whole buffer; the replies may arrive in any order and interleaved
  with the DA / XTVERSION replies.

Keep the function pure and side-effect free — it is the unit-test seam.

### 3. Add the policy function

Add an inherent method on `ProbeResult`:

```rust
/// Whether the terminal can report `Ctrl+<digit>` / `Ctrl+Alt+<digit>`
/// distinctly from a legacy control byte.
///
/// `Some(true)`  — confirmed: `modifyOtherKeys` is at level >= 2, which is
///                 what thegn's chord matching needs (termwiz pushes level 2
///                 in `set_raw_mode`, and the probe runs after that push, so
///                 this reads the level actually in effect).
/// `Some(false)` — confirmed broken: either XTQMODKEYS answered with a level
///                 below 2, or the terminal answered the kitty query but not
///                 XTQMODKEYS (a kitty-protocol-only terminal, where thegn's
///                 `modifyOtherKeys` push provably did nothing — thegn does
///                 not push the kitty protocol; see `run.rs:466-478`).
/// `None`        — cannot tell (no probe, or the terminal was silent on both
///                 queries). Callers MUST treat this as "assume it works".
pub fn ctrl_digit_reportable(&self) -> Option<bool>
```

Truth table — implement exactly this:

| `modify_other_keys`    | `kitty_keyboard` | result        |
| ---------------------- | ---------------- | ------------- |
| `Some(n)` where n >= 2 | any              | `Some(true)`  |
| `Some(n)` where n < 2  | any              | `Some(false)` |
| `None`                 | `Some(_)`        | `Some(false)` |
| `None`                 | `None`           | `None`        |

**D4 is load-bearing: `None` must never suppress anything.** Do not "helpfully"
turn an unknown into a `false`.

### 4. Add the query string constant

Export the exact bytes chunk 3 will write, so the queries and the parser live
next to each other and stay in sync:

```rust
/// The keyboard-reporting queries the startup probe writes, in order, BEFORE
/// XTVERSION + Primary DA (the DA reply is the batch terminator).
///
/// `CSI ? u`   — kitty progressive-enhancement query.
/// `CSI ? 4 m` — XTQMODKEYS (xterm modifyOtherKeys level).
/// `CSI m`     — plain SGR reset. Mandatory insurance: `CSI ? 4 m` carries a
///               private-parameter marker that a conformant parser ignores,
///               but a sloppy one could read as `SGR 4` (underline). See
///               design §4 risk 1.
pub const KEYBOARD_QUERIES: &[u8] = b"\x1b[?u\x1b[?4m\x1b[m";
```

## Tests (add to `termcaps.rs`'s existing `mod tests`)

`thegn-core` is coverage-gated at 95% lines — every branch you add needs a test.
Cover at minimum:

1. XTQMODKEYS reply `\x1b[>4;2m` ⇒ `modify_other_keys == Some(2)`,
   `ctrl_digit_reportable() == Some(true)`, `responded == true`.
2. `\x1b[>4;1m` ⇒ `Some(1)` ⇒ `ctrl_digit_reportable() == Some(false)`.
3. `\x1b[>4;0m` ⇒ `Some(0)` ⇒ `Some(false)`.
4. kitty-only: `\x1b[?0u` ⇒ `kitty_keyboard == Some(0)`,
   `modify_other_keys == None`, `ctrl_digit_reportable() == Some(false)`.
5. Silent-but-responded: a DA reply only (`\x1b[?62;c`) ⇒ both fields `None`,
   `ctrl_digit_reportable() == None`, `responded == true`.
6. Empty buffer ⇒ `ProbeResult::default()`, `ctrl_digit_reportable() == None`.
7. **DA must not be mistaken for a kitty reply**: `\x1b[?62;1;6c` alone ⇒
   `kitty_keyboard == None`.
8. **Realistic full batch, any order**: kitty reply + XTVERSION
   (`\x1bP>|ghostty 1.0\x1b\\`) + XTQMODKEYS + DA concatenated ⇒ all four
   signals recovered (`terminal_name`, `modern`, both keyboard fields).
9. **Truncated / partial buffers degrade to unknown, never to a wrong answer**
   (design §4 risk 2): e.g. `\x1b[>4;` (cut mid-number) and `\x1b[?0`
   (no terminator) ⇒ the respective field stays `None`.
10. Existing `interpret_probe` tests still pass unchanged — this must be purely
    additive.

## Tests to run (scoped — do NOT run a full-workspace gate)

```sh
just quick thegn-core
cargo nextest run -p thegn-core termcaps
```

Optionally, since this crate is the coverage-gated one and your diff is small:
`cargo llvm-cov nextest -p thegn-core --lib` is fine, but it is not required
here — chunk 3's coder runs the pre-push gate.

Do **not** run `just test`, `just ci`, `just coverage` (workspace), or `just e2e`.

## Done criteria

- `crates/thegn-core/src/termcaps.rs` is the only changed file.
- `ProbeResult` has `modify_other_keys` and `kitty_keyboard`; `interpret_probe`
  populates both; `ctrl_digit_reportable()` implements the truth table exactly;
  `KEYBOARD_QUERIES` is exported with the trailing `\x1b[m`.
- All ten test cases above exist and pass; no pre-existing test changed.
- `just quick thegn-core` is clean (clippy `-D warnings`).
- No I/O, no new dependency, no `TermCaps` change, no host file touched.

**Commit subject (exact):**

```
feat(the-70): probe the outer terminal's keyboard reporting
```

Put the design's §1.4 reasoning (one short paragraph: legacy has no distinct
Ctrl+digit byte; thegn relies on modifyOtherKeys=2; the probe now confirms it
rather than assuming) in the commit body, and note that `None` means
"assume it works".
