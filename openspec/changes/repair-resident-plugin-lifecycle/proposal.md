# Repair resident plugin I/O and lifecycle ownership

THE-154: blocking stdin writes currently hold a mutex needed by close, while the
stdout thread can hold the child mutex across an unbounded wait. This can freeze
plugin callbacks and prevent shutdown from reaching the child.

Replace those shared OS handles with bounded FIFO admission and an application
supervisor. The supervisor owns each process, pipe operation, lifecycle task and
completion receipt across reloads and shutdown. This is a bug and resiliency
repair to the existing plugin runtime; it adds no plugin capability.

Impact: plugin-runtime, resident provider correlation, host shutdown/reload, and a
narrow service platform seam. Full THE-154 acceptance remains open: process groups
and Windows direct-child handles do not prove escaped-descendant containment, and
native Windows execution is a separate outstanding gate.
