# Design

Both transports use the same limits: header including CRLFCRLF 8192 bytes; body 64 MiB; total unconsumed buffer body + header + one 8192-byte read. Append checks precede reserve/copy, and the backing vector's growth target is clamped to that bound. A cursor scans each potential delimiter once; complete-body state prevents rescanning headers while bodies arrive. Offset consumption and geometric-threshold compaction bound whole-buffer moves. Strings are emitted only after strict UTF-8 validation.

Malformed, duplicate, negative, overflowing or over-cap lengths, invalid UTF-8 and truncated EOF are sticky errors that release the buffer immediately. Extra syntactically valid headers remain supported. There is no resynchronization through unknown body boundaries. FramedReader runs only on the existing dedicated blocking reader threads, reads at most 8192 bytes and returns one frame per call. After 64 buffered frames it yields the thread before continuing without reading ahead of buffered work. No timer or new idle wake source is added.

The LSP reader drains pending requests on failure; the bridge client marks closed under its pending lock and clears pending requests, process subscribers and filesystem subscribers; agent serve stops accepting requests. Existing separate process-tree teardown/RPC admission gaps are not certified by this framing change.

Verification: seven actual-module rustc tests pass, including exact caps, bytewise scan counts, amortized moves, malformed inputs and >64 buffered messages without another read. Three consumer close regressions are checked in; integrated svc execution is pending the serialized Cargo slot.
