# Security Policy

## Supported versions

thegn is pre-1.0 alpha software. Security fixes land on `main` and in the latest
tagged release only; there is no back-port line yet.

| Version                 | Supported |
| ----------------------- | --------- |
| `main` / latest release | yes       |
| older tags              | no        |

## Reporting a vulnerability

Please report security issues **privately** — do not open a public issue, PR, or
Discussion for anything security-sensitive.

- **Preferred:** GitHub's [private vulnerability reporting](https://github.com/blakeashleyjr/thegn/security/advisories/new)
  (Security tab -> Report a vulnerability).
- **Email:** blake@ashleyjr.com

Please include enough detail to reproduce: affected version or commit, platform,
and a proof of concept if you have one.

## What to expect

- An acknowledgement within about 5 business days.
- An assessment and, where warranted, a fix or mitigation plan.
- Credit in the release notes once a fix ships, unless you ask to remain anonymous.

Because thegn spawns shells, PTYs, and sandboxed processes, and shells out to
`git`, `gh`, and `ssh`, reports about sandbox escape, credential leakage between
worktrees, or command-injection through untrusted repo content are especially
valued.
