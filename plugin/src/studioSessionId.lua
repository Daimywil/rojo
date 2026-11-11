--[[
	Returns what this Studio calls itself to Rojo servers and to the tools
	that drive the plugin, so that they can tell which Studio is which when
	several have the same place open.

	It's made up once for as long as the place stays open, and kept in an
	attribute of CoreGui, which is local to this Studio and never saved. That
	way it survives the plugin being reloaded, and tools that can run Luau in
	Studio can read it as RojoStudioSessionId.
]]

local CoreGui = game:GetService("CoreGui")
local HttpService = game:GetService("HttpService")

local ATTRIBUTE = "RojoStudioSessionId"

return function(): string
	local id = CoreGui:GetAttribute(ATTRIBUTE)
	if typeof(id) ~= "string" or #id == 0 then
		id = HttpService:GenerateGUID(false)
		CoreGui:SetAttribute(ATTRIBUTE, id)
	end

	return id
end
