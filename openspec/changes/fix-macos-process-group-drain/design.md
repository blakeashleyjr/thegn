# Design

On Darwin, `killpg(pgid, 0)` is not a drain proof while the direct child remains
unreaped: the zombie leader can keep the group observable. Reaping that leader
early would remove the pgid pin used by the existing safe group-signal path.
Instead, enumerate members of the still-pinned group with libproc and inspect
their BSD process state. `SZOMB` members do not represent running work. Any
failed, truncated, or otherwise incomplete observation remains live so the
existing bounded quiesce/termination path owns descendants and signals only
while the leader still pins the group.

Linux keeps its reap-then-probe path unchanged. Other non-Linux Unix targets
retain their existing conservative behavior.
