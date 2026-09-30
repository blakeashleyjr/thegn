# Preserve winning host environment definitions

## Why

THE-598 identifies a routing inconsistency: merging persisted host definitions
keeps a same-name declarative host but synthesizes the selectable environment
from the losing persisted definition. A declared local host can consequently
produce an SSH environment aimed at an obsolete destination.

## What changes

Synthesize placement and SSH settings from the effective winning host entry.
Preserve explicit environments and existing unsupported pane reaches. The
caller-supplied captured definitions remain the only augmentation input.

## Impact

This bounded correction supports [roadmap O.185 and O.189](../../../tasks.md)
(declarative configuration and precedence) and the configuration capability.
Owner: THE-598, Runtime Security & Host Architecture. It adds no configuration
keys, UI, transport or provider invocation. Strict DB capture (THE-602), DB
classification (THE-603), and checked final launch composition (THE-592) remain
separate acceptance obligations.
