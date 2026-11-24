# Nine programme release-channel features

## Why

The sandbox and remote programme adds nine experimental `Feature` variants at once, so that no later item
edits the shape of `Feature` or the length of `Feature::ALL`. This change records them in the
`release-channels` capability and in the release-channels help page. It changes no behaviour: every new
feature is experimental, so the stable channel never allows it, and nothing becomes configurable.

The variants and their ids are `SandboxBackends` (`sandbox_backends`), `LinuxConfine` (`linux_confine`),
`EgressProxy` (`egress_proxy`), `Seatbelt` (`seatbelt`), `WinIsolation` (`win_isolation`), `PluginBackends`
(`plugin_backends`), `AgentPresets` (`agent_presets`), `PrivateCopy` (`private_copy`) and `Escalation`
(`escalation`). `Feature::ALL` then has 16 entries, the existing seven first (including the later-added
`Lead` gate). The private-workspace gate is `PrivateCopy`; it was named `PrivateClone` in earlier drafts and
the old name appears nowhere.

## What Changes

- One ADDED requirement in the `release-channels` capability listing the nine ids as experimental, so the
  stable channel disallows them and `thegn doctor` lists 16 features, the nine new ones reading
  "experimental, not yet implemented".
- `docs/help/release-channels.md` lists the nine features.

## Non-goals

- Any gating behaviour. Each owning item (WS02, WS03, WS04, WS05, WS06, WS10, WS11) fills its clamp arm and
  replaces its doctor text.
- A catalog row, CLI verb, control op, config key or schema change.
- Hiding unimplemented features from doctor; they are listed with "not yet implemented" text.

## Impact

- **Roadmap:** `tasks.md` group BB (sandbox + remote programme), item 763; programme item WS00-P1-01.
- **Spec collision:** this change is the single owner of the `release-channels` delta for all nine features
  (B6), so no other change edits that capability's feature list. It ADDs one requirement and MODIFIES none.
- **Archive:** at the end of wave W0 (B6).
