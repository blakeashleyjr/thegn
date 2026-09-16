# Independent adversarial review: THE-286 and THE-310

Reviewer: process_refresh_investigation (not an author of either parser change).
Reviewed Kitty ea418c2e plus d34bfa4c and shared framing 27361afa plus ebf783dd.
Verdict: approved for their scoped parser/framing defects, pending the integrating
reviewer's combined host and service validation.

The shared decoder checks bounds before append, retains a monotone scan cursor,
rejects duplicate/invalid length headers and malformed/truncated bodies, and
keeps errors terminal rather than searching attacker-controlled body data for a
replacement header. Ordinary compaction moves no more than the consumed prefix;
the primary review's forced-compaction loophole is closed by ebf783dd, which
refuses a near-cap append that would repeatedly move a larger unread suffix.
FramedReader drains buffered messages before another read, yields after bounded
frame batches, and is only used on existing dedicated reader threads. Bridge and
LSP readers release current pending subscribers on stream failure. No input-loop
polling or new UI wake source was introduced.

Kitty framing retains at most one capped APC, discards oversized APCs through a
possibly split ST, then resumes ordinary parsing without leaking the discarded
tail as text. Resets drop both partial and discard state. The callback consumer
preserves text/graphics ordering. I found an 8193-byte text piece when 8191 plain
bytes were followed by ESC and an ordinary byte. The author fixed this in
d34bfa4c by flushing at the intermediate boundary and added the exact regression,
including capacity and concatenated output assertions.

Independent execution imported the actual production files directly into rustc
test binaries: Kitty 14 passed; framing 8 passed. Logs are
/tmp/thegn-kitty-independent-tests.log and /tmp/thegn-framing-independent-tests.log.
These include large bytewise APC input, split ST, oversized recovery, exact header
limit, malformed UTF-8, no re-synchronization, many buffered tiny frames, and the
hostile near-cap sliding-window regression. This is independent of the author's
reported service integration results and does not replace combined host tests.

Nonblocking pre-existing lifecycle gap: LSP's reader drains pending requests at
closure, but request() can insert a new pending entry afterward because it has
no bridge-style closed latch. If server stdin stays writable that request waits
until timeout. Track this separately; no claim that stream closure rejects all
future requests immediately is made by this verdict.

## Final THE-310 / THE-343 / THE-471 review

Reviewed e7cc883b (LSP close latch), d84adbd4 plus70a3d678 andebacd057 (OSC clipboard parser/admission/retry), andf891f015 plusbf5fcd0e/d94ca58b (calendar display). LSP pending insertion/terminal drain share a lock; post-close requests and notifications refuse admission. Clipboard vectors have explicit wire allocation limits, bounded writer FIFO, latest pending ownership, and generation-tagged backlog barriers. Retry wake fixes cover lock contention, stale ready entries and successful sync admission budgets without an idle timer.

Independent review found OSC-looking bytes inside DCS/APC/PM/SOS could be accepted by the raw PTY scanner. Author revision ebacd057 adds a constant-memory skip state through ST, including split ST and embedded BEL. The actual final parser passed6/6 standalone tests in `/tmp/thegn-osc-independent-tests.log`.

Calendar scalar/cell/control projections preserve provider/domain raw values, bound input scanning, retain legitimate combining/script shaping, and use the same label fallback in width and drawing. Late source revisions were inspected; no remaining source blocker. Full host integration fixtures remain the final gate, not claimed by this source review.
