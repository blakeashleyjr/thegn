# Reduce process monitor idle work

THE-630/631/632 repair existing task-manager performance and refresh behavior:
the process thread wakes every 500ms while hidden and has no retained owner;
unrelated stats rebuild every row set; changing values shift process columns.

Use a fallible owned sampler with an interruptible indefinite park, bounded
latest publication and acknowledged cancellation. Then cache only the active
monitor's relevant row/view inputs, preserving graph time and disk age updates,
and constrain only the Processes table to viewport-derived cell widths.

No new collectors, settings, controls or features. THE-633 hydration provider
probing is a separate implementation lane. Impact: existing system-monitor
capability, host runtime cleanup, metrics sampling and opt-in table rendering.
