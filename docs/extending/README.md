# Extending thegn

One recipe per surface. Each ends with **the gate**: the test or lint that
fails if you skip a step, so "did I forget something?" has a mechanical
answer. Architecture context: [`../ARCHITECTURE.md`](../ARCHITECTURE.md).

| I want to add…                                         | Recipe                                 |
| ------------------------------------------------------ | -------------------------------------- |
| a config key                                           | [config-key.md](config-key.md)         |
| an action (keybind / palette row)                      | [action.md](action.md)                 |
| a help page                                            | [help-page.md](help-page.md)           |
| a theme preset                                         | [theme.md](theme.md)                   |
| a CLI subcommand                                       | [cli-subcommand.md](cli-subcommand.md) |
| a provider (forge / CI / tracker / …)                  | [provider-impl.md](provider-impl.md)   |
| a plugin (out of process, or a plugin-backed provider) | [plugin.md](plugin.md)                 |
| a host capability (API / MCP / plugin verb)            | [capability.md](capability.md)         |
