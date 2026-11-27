# Authenticate CI and contributor bootstrap inputs

Linear: THE-413

## Problem

Bootstrap scripts and release downloads can select mutable executable bytes at
setup time. HTTPS protects transport but does not establish that downloaded
bytes are the version reviewed by this repository.

## Scope

Pin the Linux Actions runner archive, authenticate the existing Nix installer,
and authenticate the bare macOS rustup bootstrap before any artifact is
executed, extracted, or installed. Re-authenticate cached runner archives on
every setup run and validate archive members before extraction. Record the
reviewed pin refresh procedure and exercise refusal paths with offline fixtures.
