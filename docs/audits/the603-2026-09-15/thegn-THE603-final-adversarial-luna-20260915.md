# THE603 final standalone adversarial review — Luna

Review date: 2026-09-15. This is an independent source and evidence review;
no build, test, native rerun, live database, provider, permission mutation,
candidate edit, or Linear write was performed.

## Verdict

**Approved with explicit scope limits; no implementation blocker found.** The
standalone candidate is `38a1f84d997bf68b7282114113817b01aa3ea635`, based on
`59a9c944bcc94107ded859932864ffaf9be1ffb3`. The source-equivalence receipt
matches all five new THE603 files and the three THE602 dependencies byte for
byte against corrected tested source `dd5e4d07ff16428f250388b142c5f9b4c25561c3`.
The final Linux filesystem-magic normalization is semantically equivalent to
the tested source and is included in that hash bridge.

The component is additive and unwired. It does not integrate THE592 startup,
launch authority, worker scheduling, or inode-pinned SQLite. The core helper
uses read-only Unix-VFS flags, the existing strict THE602 transaction, a 250ms
busy timeout, copied results, and explicit connection close. The Linux helper
retains O_PATH ancestor descriptors, validates regular types/ownership/modes
and supported filesystem kinds, rechecks missing observations and sidecars,
and returns typed refusal categories. The source comments and OpenSpec
correctly state that this is a detector with a stable-namespace assumption,
not an atomic SQLite-inode or hostile same-UID replacement guarantee.

## Positive evidence and source bridge

The current06 focused receipt at source `27c0ccad5399c84612fa35aea7841b0d4209458c`
reports 121/121 passed and 8220 filtered, including the five core THE603
capture tests, 16 Linux tests, and THE602 compatibility coverage. Its receipt
SHA-256 is `374ac6cf961718a9db82f022108583b4adcf04b7a6b22e755fd12c7635dfb7a2`;
its log SHA-256 is
`94e6693c851cc1d62de35c092483b5d6fc26110b91f728826b7b89e9b5cf88c3`.

The corrected Linux receipt directly identifies tested source `dd5e4d07`,
reports 16/16 passed in 0.747 seconds, and has receipt SHA-256
`1351eb903a054ce2e04bf8ca14f4b5891762eb563f6302f7307333e360fbb241`; its
log SHA-256 is
`f642cc17e6217b807f1a8533e5ef1811ad5668b9e21887fa3d54caf9719d12c1`.
The corrected strict clippy receipt reports exit 0 with log SHA-256
`bf7f8cd96cdf9f7b7c1df3405bfe00715327ad9da9ad90c8f5cd686c7f1d9b340c4`
as recorded by the supplied gate receipt (the receipt itself is the authority
for that path/hash pair). Source ratchets, strict OpenSpec and fmt all passed
in the standalone candidate: their gate-manifest SHA-256 is
`3c3ac741f1b6bd6c3092edf7534f5cc9551a3bf5fb4ebd532409ea730de415bb`.

These receipts are reused only through the exact source hash bridge; they do
not claim a fresh full-workspace run for candidate `38a1f84d`. Linux evidence
does not establish Windows, Darwin, or production `/tmp` acceptance. The
focused receipt's historical `independent_review: pending` field is superseded
for this review by this report, without modifying the receipt.

## Counterfactual 1 — missing ancestor verification

The one-line deletion removes `self.chain.verify(true)?` from
`MissingObservation::verify`, leaving final missing-name and sidecar checks
intact. The exact private mutant source is commit
`3d3271deb5f9c2fcc6d5b7b5ef18f36d938f6381`; its Linux source hash is
`5854949eab555983998018c647c7357ef47d76c60b5c0fb675da256513732b0a`.

The exact selector ran once and exited 101 after 0.07 seconds. The first
`change_mode = false` replacement case reaches the rendezvous, `reads == 0`
assertion at line 440, then fails the intended `matches!(result,
Err(Error::Changed))` assertion at line 441 because the mutant returns
`Capture::Absent`. No SQLite read occurs. The mode-change case, fresh recovery
capture, and strict `TempDir::close` are later and unreached; only ordinary
fixture Drop runs during the panic. This is an expected semantic mutant kill,
not a cleanup-success claim.

Receipt provenance is internally consistent: receipt SHA-256
`b62bae231a1f431698df83420a400bd0c387216680ec25da5f49e764aa68e87e`, binary
SHA-256 `b4edf4dd1fbe101ac2d44969c7efe01ca80bedf5d3eaf844d6465a7a56bbe058`,
depfile SHA-256
`d8e76a3ae0e42c39d316843524a28588bd0b6cfdb83bfca1bdd0c5544352a236`, and
native log SHA-256
`22ee7ed9baa10dc4d0347c8ac5d3fdf70ce59224d2d3e699c7b44371da9b8aed` all
match their files. The build exited 0 with a non-fresh artifact, and the
depfile binds the named mutant checkout.

## Counterfactual 2 — orphan sidecar refusal

The one-line replacement changes the regular-orphan branch from
`Ok(_) => return Err(Error::OrphanedSidecar)` to `Ok(_) => {}`. O_PATH and
no-follow inspection remain in place; the opened orphan descriptor is dropped
without data or device I/O. The exact mutant source is commit
`7133a556d8897c14ec9b278a53bcf770cc01c707`, with Linux source hash
`0568fe6f44434bd77c286a924c8be19f603074c599bb0655fa6c52104a1fa77f`.

The exact selector ran once and exited 101 after 0.07 seconds. The first
regular `state.db-wal`, `late = false` case reaches the `reads == 0` assertion
at line 470 and then fails `result.unwrap_err()` at line 471 because the
mutant returns `Ok(Capture::Absent)`. Later suffixes, late-creation,
symlink/FIFO cases, and strict directory close are unreached; ordinary fixture
Drop is the only unwind cleanup. No SQLite read, FIFO data open, or orphan
deletion occurs. This is the intended semantic failure and does not claim
normal-success cleanup for the aborted test.

Receipt provenance matches: receipt SHA-256
`9f8c5a861196f9493a6a5eca9df21399113105c4c09615ef95b6b089a13e04de`, binary
SHA-256 `c1ba01d402273417003a755b0d74cee475d93903733b5af061f754c0e5bd3adb`,
depfile SHA-256
`d45994787bafd0d37e878fd6685b5664c885663afe85b7d4525a33c7d35bc86d`, and
native log SHA-256
`495002ca09d65d68d01e23e813c06159daa3b1b312f7a0d354dbedf3f99db8ae` all
match. The build exited 0 with a non-fresh artifact, and the depfile binds the
orphan mutant checkout.

The two counterfactuals therefore prove their targeted source boundaries with
the required first assertions and no SQL or special-file side effects. They do
not provide cleanup-success evidence because each intentionally aborts before
its strict `TempDir::close`; root should retain that reachability limit in the
final inventory.

## Remaining documentation qualification

The candidate audit still contains historical checkpoint wording naming the
older `bb97ec24`/`156ed688` ancestry and says current native/lint work is
pending. That wording is a metadata freshness issue for root to reconcile with
the candidate `38a1f84d`, source-equivalence receipt, and corrected gates. It
does not contradict the implementation scope or create a code blocker. No
issue closure, activation, provider requirement, or inode-pinning claim is
made here.
