--[[
	Finds the Rojo servers running on a machine, so the panel can offer them to
	connect to instead of having their ports typed in.

	Every Rojo server registers itself, and one of them always listens on the
	discovery port to list all of them (see src/server_registry.rs on the
	server). Each server it lists is then asked about itself, which tells
	whether it can be used with this place.

	Older Rojo servers don't register, so the ports this place is known to have
	used, like the one in the panel, are asked directly too.
]]

local Packages = script.Parent.Parent.Packages
local Http = require(Packages.Http)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)

-- How long a port may take to answer. Something other than Rojo listening on
-- a port may never answer at all.
local TIMEOUT = 2

local ServerDiscovery = {}

export type Server = {
	host: string,
	port: string,
	projectName: string,
	-- Whether the server's project is meant for this place: "this" if it lists
	-- this place, "other" if it can't be used with this place, and nil if it
	-- doesn't say.
	place: ("this" | "other")?,
	-- Why connecting to it can't work, if it can't.
	unavailable: string?,
}

local function baseUrl(host: string, port: string): string
	if string.find(host, "^https?://") then
		return string.format("%s:%s", host, port)
	end
	return string.format("http://%s:%s", host, port)
end

local function placeOf(info): ("this" | "other")?
	if typeof(info.expectedPlaceIds) == "table" then
		return if table.find(info.expectedPlaceIds, game.PlaceId) then "this" else "other"
	end
	if typeof(info.unexpectedPlaceIds) == "table" and table.find(info.unexpectedPlaceIds, game.PlaceId) then
		return "other"
	end
	if info.placeId ~= nil and info.placeId == game.PlaceId then
		return "this"
	end
	return nil
end

local function get(host: string, port: string, path: string)
	return Http.get(baseUrl(host, port) .. path):andThen(Http.Response.msgpack):timeout(TIMEOUT)
end

--[[
	Turns a port into a string if it's a valid port, or nil if it isn't.
]]
function ServerDiscovery.normalizePort(port: any): string?
	local number = tonumber(port)
	if number == nil or number % 1 ~= 0 or number < 1 or number > 65535 then
		return nil
	end
	return tostring(number)
end

--[[
	Resolves with the ports of the servers that `host` has registered, asking
	the discovery port and the given ports, any of which may answer. Never
	rejects: when nothing answers, there are no registered servers.
]]
local function registeredPorts(host: string, askPorts: { string })
	local requests = {}
	for _, port in askPorts do
		table.insert(
			requests,
			get(host, port, "/api/servers"):catch(function()
				return nil
			end)
		)
	end

	return Promise.all(requests):andThen(function(responses)
		local ports = {}
		for _, response in responses do
			if typeof(response) == "table" and typeof(response.servers) == "table" then
				for _, server in response.servers do
					local port = if typeof(server) == "table" then ServerDiscovery.normalizePort(server.port) else nil
					if port ~= nil then
						table.insert(ports, port)
					end
				end
			end
		end
		return ports
	end)
end

--[[
	Asks a port whether it's a Rojo server. Resolves with what's known about
	the server, or nil if it isn't one.
]]
local function probe(host: string, port: string)
	return get(host, port, "/api/rojo")
		:andThen(function(info): Server?
			if typeof(info) ~= "table" or typeof(info.projectName) ~= "string" then
				return nil
			end

			local place = placeOf(info)
			local unavailable = nil
			if info.protocolVersion ~= Config.protocolVersion then
				unavailable = "Different Rojo version"
			elseif place == "other" then
				unavailable = "Not for this place"
			end

			return {
				host = host,
				port = port,
				projectName = info.projectName,
				place = place,
				unavailable = unavailable,
			}
		end)
		:catch(function()
			return nil
		end)
end

--[[
	Every port in the given lists that's a valid port, once, in order.
]]
function ServerDiscovery.uniquePorts(...: { any }): { string }
	local ports, seen = {}, {}
	for index = 1, select("#", ...) do
		for _, port in (select(index, ...)) do
			local normalized = ServerDiscovery.normalizePort(port)
			if normalized ~= nil and not seen[normalized] then
				seen[normalized] = true
				table.insert(ports, normalized)
			end
		end
	end
	return ports
end

--[[
	Resolves with every Rojo server found on `host`: the registered ones, and
	any listening on `knownPorts`. Servers meant for this place come first,
	then the rest, each by port.
]]
function ServerDiscovery.find(host: string, knownPorts: { any })
	local askPorts = ServerDiscovery.uniquePorts({ Config.discoveryPort }, knownPorts)

	return registeredPorts(host, askPorts)
		:andThen(function(registered)
			local probes = {}
			for _, port in ServerDiscovery.uniquePorts(registered, knownPorts) do
				table.insert(probes, probe(host, port))
			end
			return Promise.all(probes)
		end)
		:andThen(function(results)
			local servers: { Server } = {}
			for _, server in results do
				if server ~= nil then
					table.insert(servers, server)
				end
			end

			local rank = { this = 0, other = 2 }
			table.sort(servers, function(a, b)
				local rankA, rankB = rank[a.place] or 1, rank[b.place] or 1
				if rankA ~= rankB then
					return rankA < rankB
				end
				return tonumber(a.port) < tonumber(b.port)
			end)

			return servers
		end)
end

--[[
	Whether two lists of servers show the same thing, so the panel doesn't
	re-render when nothing changed.
]]
function ServerDiscovery.same(a: { Server }?, b: { Server }?): boolean
	if a == nil or b == nil then
		return a == b
	end
	if #a ~= #b then
		return false
	end

	for index, server in a do
		local other = b[index]
		if
			server.host ~= other.host
			or server.port ~= other.port
			or server.projectName ~= other.projectName
			or server.place ~= other.place
			or server.unavailable ~= other.unavailable
		then
			return false
		end
	end

	return true
end

return ServerDiscovery
