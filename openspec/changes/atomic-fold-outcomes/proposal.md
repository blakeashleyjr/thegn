# Atomic final fold outcomes

THE-591 complements THE-589 and roadmap group T item 758. Publishing queued then
held in separate writes permits a concurrent driver to observe a runnable state.
Add an atomic, observed-state final outcome operation to the existing store seam.
Both integration entrypoints capture observations before Git/gates, project final
rows once, and apply lifecycle only after the final row commits.

This repair blocks final acceptance of THE-589. THE-588 independently owns
destructive filesystem cleanup safety; THE-586 native retry depends on both.
