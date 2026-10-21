# Design

Text, trailing ESC, APC and discard states inspect each newly received byte once. One geometric-growth vector retains an APC including its introducer/terminator, with an exact 4 MiB cap. An oversized APC releases its allocation and discards all bytes through ST, including split ST. Completed sequence ownership moves to classification without a payload clone. Classification examines only completed control fields.

A callback emits pieces immediately; arbitrary large input cannot accumulate an additional result vector. Ordinary text emits in chunks of at most 8192 bytes (an ESC pair flushes between its bytes at the exact threshold). One retained APC plus one text chunk bounds parser-owned memory. Downstream graphics queues and multi-command transfer transactions retain their existing independent ownership; this repair does not certify those queue bounds. Reset releases incomplete/discarded state at pane replacement.

Validation: actual production module compiled with rustc --test; 14 tests pass, including every split, exact maximum under one-byte/small/normal chunks, oversize recovery, bounded text emission and reset. Integrated host compilation and independent review remain pending.

Independent reviewer required an exact 8191-byte text prefix plus ESC-pair capacity regression; the parser now flushes at 8192 before appending the second byte.
