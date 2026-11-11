# Control API worked examples

Request and error bodies for the [control API](control-api.md). Every example
below is exercised by `crates/thegn-svc/tests/control_docs_examples.rs`, which
deserializes it into the wire type named in its tag and checks that the route is
registered; do not edit one without running that test.

`POST /v1/pair` redeems a pairing code for a token. It is unauthenticated: the
single-use code is the credential, so no scope is needed.

<!-- example: POST /v1/pair as PairBody -->

```json
{ "code": "tgp1_example", "label": "phone" }
```

`POST /v1/sessions/{s}/input` types into a session and, with `enter`, sends a
carriage return after it. It needs the `write` scope.

<!-- example: POST /v1/sessions/{s}/input as InputBody -->

```json
{ "text": "ls", "enter": true }
```

`POST /v1/sessions/{s}/resize` sets a session's terminal geometry. It needs the
`write` scope.

<!-- example: POST /v1/sessions/{s}/resize as ResizeBody -->

```json
{ "rows": 40, "cols": 120 }
```

`POST /v1/git/stage` stages paths in a worktree through the git backend. It
needs the `git` scope.

<!-- example: POST /v1/git/stage as StageBody -->

```json
{ "worktree": "/wt/a", "paths": ["src/main.rs"] }
```

`POST /v1/git/commit` commits what is staged in a worktree. It needs the `git`
scope.

<!-- example: POST /v1/git/commit as CommitBody -->

```json
{ "worktree": "/wt/a", "message": "fix" }
```

Every failed request carries the same envelope: a human message in `error` and
a stable machine code in `code`. This one is what a missing session returns, and
no scope is needed to read it.

<!-- example: error envelope -->

```json
{ "error": "no such session", "code": "not_found" }
```
