# THE-621 native origin acceptance

The existing native host check admitted a foreign URL whose path contained
`@github.com`. The reviewed repair uses the existing strict repository-origin
parser once for both host admission and owner/name identity. Only supported
public GitHub origins reach token lookup. Unsupported forms retain CLI fallback.

Primary review reproduced the old parser's incorrect identity using exact source
functions. Independent adversarial review approved the minimal two-file change
and verified that the actual-Git test cannot pass through an unrelated circuit
refusal or a blanket denial of every origin.

Source `5e2f5bf2bcb3b2473e48775850133983e1f7be31` passed 26/26 native forge tests
(668 unselected), including the new fixture's 18 negative and four positive
origin cases. Strict service-package/all-targets Clippy passed. The native test
suite retains ladder-selection, SDK fallback and credential ownership tests.
The new origin fixture uses private Git state and an inert injected token;
it never sends a native request or reads real credentials.

Evidence is recorded in the adjacent `THE-621-origin-artifacts/` directory.
Actual local-main landing and final source-only gates remain pending here.
These scoped results do not claim another full-workspace test run.

The broader `forge::remote_host` routing defect is tracked as THE-642. Some Git
URL forms, including explicit ports and HTTPS userinfo, intentionally fall back
to the CLI because the native parser accepts a narrower, verified grammar.
No remote push, live restart or live state migration is part of this repair.
