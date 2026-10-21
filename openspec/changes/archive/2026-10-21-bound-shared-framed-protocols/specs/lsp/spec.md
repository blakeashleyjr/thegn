## ADDED Requirements

### Requirement: Bounded shared framed decoding

LSP and bridge readers SHALL use bounded header, body, buffer and dispatch work limits and SHALL terminate a malformed framed stream without guessing a new frame boundary.

#### Scenario: Unterminated header

- **WHEN** the peer sends a header with no separator up to the header cap
- **THEN** decoding SHALL return a typed terminal protocol error and release buffered bytes

#### Scenario: Bytewise delivery

- **WHEN** a valid frame arrives one byte at a time
- **THEN** decoding SHALL examine input with amortized linear scan and move work

#### Scenario: Many buffered messages

- **WHEN** more than 64 complete frames are buffered
- **THEN** the blocking reader SHALL yield between bounded batches without losing buffered frames or waiting for more input

#### Scenario: Protocol closure

- **WHEN** the decoder rejects malformed framing
- **THEN** LSP and bridge clients SHALL close their reader and release existing pending request waiters, and the bridge agent SHALL stop accepting following requests
