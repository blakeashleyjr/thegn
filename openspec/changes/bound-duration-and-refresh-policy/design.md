# Design

Use `time_policy` for nonzero cadence slots, unsigned age/freshness, checked
provider deadlines, and wide-domain duration-to-millisecond conversion. Convert
through u128 and use ceiling division so a scheduler cannot wrap to zero or fire
before a nonintegral slot boundary. Every runtime entry remains safe when a
programmatic configuration bypasses strict validation.

Operational strict maxima are 31 days for periodic cadences and ten 365-day
years for other durations. Schema fields express maxima in their own units:
seconds, milliseconds, or days. Existing defaults and lower floors remain;
feature disable/inheritance semantics stay explicit (None or the field's
established zero convention). Epoch fields are never capped as durations.
The complete field-family inventory is in
`docs/audits/time-policy-duration-inventory.md`.

A blanket new serde rejection is unsafe here: `Config::load_layered` falls back
to the entire default configuration on deserialization errors. A new duration
error must not discard unrelated sandbox isolation settings. Therefore schema
numeric maxima drive duration-only strict validation, explicit overlay admission,
and pre-write validation. The permissive base-file path retains parsed settings
and warns. Runtime periodic conversion caps unsupported cadence values; invalid
destructive duration remains raw until the reaper quarantines it. This is targeted
duration recovery and does not fix the general loader-admission issue THE-505.
A CLI/profile overlay with a newly invalid duration is rejected atomically; an
unrelated repair remains possible when a base already contains diagnosed errors.

The shared ticker gets an unwind-only notification guard: exactly one error
and existing terminal wake on panic, no heartbeat poll or extra timer. Ordinary
channel shutdown remains silent. This reports worker loss rather than implying
that a background thread's death is healthy idle state.

THE-484 work must distinguish trustworthy provider creation time from unknown,
malformed, future, or overflowing data. Unknown inventory is visibly quarantined
with a reconciliation instruction; it never authorizes deletion. Existing ledger
created_at values conflate provider time and fallback local now, so they cannot
be retroactively promoted to generation-bound creation evidence. A local fallback
is only valid when genuine create-intent evidence is tied to the exact current
provider resource generation; otherwise quarantine remains necessary. No live
provider call is used during this work.

Repeated hydration must not serialize an entire Config merely to validate an
unchanged environment. Base duration diagnostics use a bounded32-entry source
fingerprint cache (diagnostics only, never runtime authorization). The environment
DTO checks its ten duration fields directly, with parity fixtures against full
schema validation. CLI/profile changes validate a small explicit patch. Schema
and full-config walks remain at strict admission boundaries or changed base source.

Model and PR due decisions are independent. A due PR subsumes a coincident model
refresh; hostile model intervals cannot starve an ordinary PR schedule. Nine
actual-source standalone cadence/schedule/panic fixtures pass in both debug and
optimized builds; final updated core admission tests and combined host remain pending.

Adversarial review found whole-environment rejection could drop a valid stronger
isolation/network override alongside a bad TTL. Environment checks now clear only
invalid duration fields and apply every other explicit setting. A mixed security
override regression covers this. Raw duration admission recognizes the two existing
metrics duration aliases, so aliases cannot bypass write/profile range checks.


THE-484 implements checked provider resource expiry with an explicit observable
quarantine decision. Fly uses its existing authenticated read seam and requires
one exact owned machine plus provider created_at; local ledger times are never
promoted to authoritative age. Empty/multiple/malformed/mismatched inventories
and transient reads retain ownership state for reconciliation. Last-read-to-delete
identity races remain outside this numerical repair. VPS parser outputs feed the
same checked age policy; missing and future timestamps do not authorize cleanup.

The remaining consumer audit includes lease/breaker/budget configuration, budget
storage window arithmetic, CI/calendar/weather/scan/LOC/Git/placement freshness,
usage and proxy reset delays, jitter and epoch narrowing, and MPRIS durations.
Strict pairing-code relative durations reject before mint/persistence. Local
provisioning/hibernation timestamps are checked before age-derived destructive
recovery. Complete file/cast classification and evidence live in the THE-484 audit.
