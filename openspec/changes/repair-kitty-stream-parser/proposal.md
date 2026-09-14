# Repair bounded Kitty APC stream parsing

THE-286 identifies quadratic prefix copying/scanning and a missing byte bound in the current corner graphics relay. Carry parser state between PTY chunks and emit completed pieces incrementally. Existing query and graphics classification semantics remain unchanged; multi-command transfer atomicity (THE-218) and outer reply suppression (THE-345) remain separate issues.

Impact: roadmap A.2 native terminal rendering; terminal-compat. The coordinating reviewer owns reciprocal delivery registration.
