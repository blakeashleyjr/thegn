--- thegn drawer control. Emits a private OSC 5379 "close" on yazi's own PTY;
--- the thegn host sniffs it (see thegn-host/src/queries.rs `drawer_command`)
--- and hides the drawer into its keep-alive pool, so yazi keeps running and its
--- cursor position survives the next open. Bundled + seeded by
--- thegn-core/src/yazi.rs alongside the pinned yazi; do not edit by hand.
--- @since 26.5.6
return {
	entry = function()
		io.write("\27]5379;close\7")
		io.flush()
	end,
}
