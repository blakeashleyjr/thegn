# Validate tracker identities at every ingress

Linear: THE-322 (Client API & Remote Access)

Issue identifiers arrive from provider responses, config, cached rows, plugin
bridges, CLI arguments, and control paths. Today several adapters interpolate
those values into URLs or `gh` options and silently turn malformed scoped
GitHub identities into bare issue numbers. This change adds bounded,
provider-aware syntax admission and encodes a complete control identity once
before it becomes a path segment.

The change preserves existing bare GitHub/Jira/Kaneo forms and enterprise
GitHub host selection. Plugin native keys remain opaque bounded UTF-8 values;
THE-324 continues to own account and generation authority.

## Non-goals

This does not select an account, authenticate a provider, add provider
features, or change the router's authority policy. Live provider calls and
native execution remain downstream validation gates.
