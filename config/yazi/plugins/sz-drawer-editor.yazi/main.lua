--- thegn drawer control. Emits a private OSC 5379 "editor;<abs-path>" for the
--- hovered file on yazi's own PTY; the thegn host sniffs it (see
--- thegn-host/src/queries.rs `drawer_command`) and opens that file in a fresh
--- center editor tab. Bundled + seeded by thegn-core/src/yazi.rs alongside the
--- pinned yazi; do not edit by hand.
--- @since 26.5.6
local hovered = ya.sync(function()
	local h = cx.active.current.hovered
	return h and tostring(h.url) or nil
end)

return {
	entry = function()
		local url = hovered()
		if url and url ~= "" then
			io.write("\27]5379;editor;" .. url .. "\7")
			io.flush()
		end
	end,
}
