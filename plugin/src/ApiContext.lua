local Packages = script.Parent.Parent.Packages
local HttpService = game:GetService("HttpService")
local Http = require(Packages.Http)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)
local Types = require(script.Parent.Types)
local Version = require(script.Parent.Version)

local validateApiInfo = Types.ifEnabled(Types.ApiInfoResponse)
local validateApiRead = Types.ifEnabled(Types.ApiReadResponse)
local validateApiSocketPacket = Types.ifEnabled(Types.ApiSocketPacket)
local validateApiSerialize = Types.ifEnabled(Types.ApiSerializeResponse)
local validateApiRefPatch = Types.ifEnabled(Types.ApiRefPatchResponse)

local function rejectFailedRequests(response)
	if response.code >= 400 then
		local message = string.format("HTTP %s:\n%s", tostring(response.code), response.body)

		return Promise.reject(message)
	end

	return response
end

local function rejectWrongProtocolVersion(infoResponseBody)
	if infoResponseBody.protocolVersion ~= Config.protocolVersion then
		local message = (
			"Found a Rojo dev server, but it's using a different protocol version, and is incompatible."
			.. "\nMake sure you have matching versions of both the Rojo plugin and server!"
			.. "\n\nYour client is version %s, with protocol version %s. It expects server version %s."
			.. "\nYour server is version %s, with protocol version %s."
			.. "\n\nGo to https://github.com/rojo-rbx/rojo for more details."
		):format(
			Version.display(Config.version),
			Config.protocolVersion,
			Config.expectedServerVersionString,
			infoResponseBody.serverVersion,
			infoResponseBody.protocolVersion
		)

		return Promise.reject(message)
	end

	return Promise.resolve(infoResponseBody)
end

local function rejectWrongPlaceId(infoResponseBody)
	if infoResponseBody.expectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.expectedPlaceIds, game.PlaceId)

		if not foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.expectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to only be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'servePlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	if infoResponseBody.unexpectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.unexpectedPlaceIds, game.PlaceId)

		if foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.unexpectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to not be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to not be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'blockedPlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	return Promise.resolve(infoResponseBody)
end

local ApiContext = {}
ApiContext.__index = ApiContext

function ApiContext.new(baseUrl)
	assert(type(baseUrl) == "string", "baseUrl must be a string")

	local self = {
		__baseUrl = baseUrl,
		__sessionId = nil,
		__messageCursor = -1,
		__wsClient = nil,
		__connected = true,
		__activeRequests = {},
	}

	return setmetatable(self, ApiContext)
end

function ApiContext:__fmtDebug(output)
	output:writeLine("ApiContext {{")
	output:indent()

	output:writeLine("Connected: {}", self.__connected)
	output:writeLine("Base URL: {}", self.__baseUrl)
	output:writeLine("Session ID: {}", self.__sessionId)
	output:writeLine("Message Cursor: {}", self.__messageCursor)

	output:unindent()
	output:write("}")
end

function ApiContext:disconnect()
	self.__connected = false
	for request in self.__activeRequests do
		Log.trace("Cancelling request {}", request)
		request:cancel()
	end
	self.__activeRequests = {}

	if self.__wsClient then
		Log.trace("Closing WebSocket client")
		self.__wsClient:Close()
	end
	self.__wsClient = nil

	self:disconnectAssetsWebSocket()
end

function ApiContext:setMessageCursor(index)
	self.__messageCursor = index
end

function ApiContext:connect()
	local url = ("%s/api/rojo"):format(self.__baseUrl)

	return Http.get(url)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(rejectWrongProtocolVersion)
		:andThen(function(body)
			assert(validateApiInfo(body))

			return body
		end)
		:andThen(rejectWrongPlaceId)
		:andThen(function(body)
			self.__sessionId = body.sessionId

			return body
		end)
end

function ApiContext:read(ids)
	local url = ("%s/api/read/%s"):format(self.__baseUrl, table.concat(ids, ","))

	return Http.get(url):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		assert(validateApiRead(body))

		return body
	end)
end

function ApiContext:write(patch)
	local url = ("%s/api/write"):format(self.__baseUrl)

	local updated = {}
	for _, update in ipairs(patch.updated) do
		local fixedUpdate = {
			id = update.id,
			changedName = update.changedName,
		}

		if next(update.changedProperties) ~= nil then
			fixedUpdate.changedProperties = update.changedProperties
		end

		table.insert(updated, fixedUpdate)
	end

	-- Only add the 'added' field if the table is non-empty, or else the msgpack
	-- encode implementation will turn the table into an array instead of a map,
	-- causing API validation to fail.
	local added
	if next(patch.added) ~= nil then
		added = patch.added
	end

	local body = {
		sessionId = self.__sessionId,
		removed = patch.removed,
		updated = updated,
		added = added,
	}

	body = Http.msgpackEncode(body)

	return Http.post(url, body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(responseBody)
			Log.info("Write response: {:?}", responseBody)

			return responseBody
		end)
end

function ApiContext:connectWebSocket(packetHandlers)
	local url = ("%s/api/socket/%s"):format(self.__baseUrl, self.__messageCursor)
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	return Promise.new(function(resolve, reject)
		local success, wsClient =
			pcall(HttpService.CreateWebStreamClient, HttpService, Enum.WebStreamClientType.WebSocket, {
				Url = url,
			})
		if not success then
			reject("Failed to create WebSocket client: " .. tostring(wsClient))
			return
		end
		self.__wsClient = wsClient

		local closed, errored, received

		received = self.__wsClient.MessageReceived:Connect(function(msg)
			local data = Http.msgpackDecode(msg)
			if data.sessionId ~= self.__sessionId then
				Log.warn("Received message with wrong session ID; ignoring")
				return
			end

			assert(validateApiSocketPacket(data))

			Log.trace("Received websocket packet: {:#?}", data)

			local handler = packetHandlers[data.packetType]
			if handler then
				local ok, err = pcall(handler, data.body)
				if not ok then
					Log.error("Error in WebSocket packet handler for type '%s': %s", data.packetType, err)
				end
			else
				Log.warn("No handler for WebSocket packet type '%s'", data.packetType)
			end
		end)

		closed = self.__wsClient.Closed:Connect(function()
			closed:Disconnect()
			errored:Disconnect()
			received:Disconnect()

			if self.__connected then
				reject("WebSocket connection closed unexpectedly")
			else
				resolve()
			end
		end)

		errored = self.__wsClient.Error:Connect(function(code, msg)
			closed:Disconnect()
			errored:Disconnect()
			received:Disconnect()

			reject("WebSocket error: " .. code .. " - " .. msg)
		end)
	end)
end

function ApiContext:open(id)
	local url = ("%s/api/open/%s"):format(self.__baseUrl, id)

	return Http.post(url, ""):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		return nil
	end)
end

function ApiContext:serialize(ids: { string })
	local url = ("%s/api/serialize"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return Http.post(url, request_body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiSerialize(response_body))

			return response_body
		end)
end

--[[
	Sends a msgpack request to one of the asset sync endpoints and returns its
	decoded response.
]]
function ApiContext:__assetsRequest(endpoint: string, body)
	local url = ("%s/api/assets/%s"):format(self.__baseUrl, endpoint)
	body.sessionId = self.__sessionId

	return Http.post(url, Http.msgpackEncode(body))
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(responseBody)
			if responseBody.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			return responseBody
		end)
end

--[[
	Lists the asset directories synced into instances.
]]
function ApiContext:assets()
	local url = ("%s/api/assets"):format(self.__baseUrl)

	return Http.get(url)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(body)
			if body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			return body
		end)
		:catch(function(err)
			-- Servers from before asset sync existed don't have this endpoint,
			-- and have no assets to sync either.
			if
				typeof(err) == "table"
				and err.type == Http.Error.Kind.Unknown
				and string.find(err.message, "Unknown HTTP error: 404:", 1, true) ~= nil
			then
				return {
					sessionId = self.__sessionId,
					messageCursor = 0,
					containers = {},
				}
			end

			return Promise.reject(err)
		end)
end

function ApiContext:assetsPlan(request: {
	clientId: string,
	placeId: string,
	session: string?,
	container: string,
	assets: { { path: string, hash: buffer } },
	skipped: { string },
})
	return self:__assetsRequest("plan", request)
end

function ApiContext:assetsStage(request: {
	clientId: string,
	container: string,
	assets: { { path: string, data: buffer } },
})
	return self:__assetsRequest("stage", request)
end

function ApiContext:assetsApply(request: {
	clientId: string,
	placeId: string,
	container: string,
	toFs: { string },
	deleteFs: { string },
})
	return self:__assetsRequest("apply", request)
end

function ApiContext:assetsAck(request: {
	placeId: string,
	container: string,
	applied: { { path: string, fsHash: buffer, studioHash: buffer } },
	removed: { string },
})
	return self:__assetsRequest("ack", request)
end

function ApiContext:assetsWrite(request: {
	clientId: string,
	placeId: string,
	container: string,
	ops: { { kind: string, path: string, data: buffer?, to: string? } },
})
	return self:__assetsRequest("write", request)
end

--[[
	Lists what a place synced before and the asset files on disk, so assets
	can keep their file names across sessions.
]]
function ApiContext:assetsKnown(request: { placeId: string, container: string })
	return self:__assetsRequest("known", request)
end

--[[
	Remembers the hashes of groups of assets that are in sync, so the next
	session can tell they didn't change without serializing them one by one.
]]
function ApiContext:assetsGroups(request: {
	placeId: string,
	container: string,
	groups: { { hash: buffer, units: { buffer } } },
})
	return self:__assetsRequest("groups", request)
end

function ApiContext:assetsSession(request: { placeId: string, session: string })
	return self:__assetsRequest("session", request)
end

function ApiContext:assetsRead(request: { container: string, paths: { string } })
	return self:__assetsRequest("read", request)
end

--[[
	Streams changes to asset files. The returned promise resolves when the
	connection is closed on purpose and rejects if it fails.
]]
function ApiContext:connectAssetsWebSocket(cursor: number, onPacket: (any) -> ())
	local url = ("%s/api/assets/socket/%s"):format(self.__baseUrl, cursor)
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	return Promise.new(function(resolve, reject)
		local success, wsClient =
			pcall(HttpService.CreateWebStreamClient, HttpService, Enum.WebStreamClientType.WebSocket, {
				Url = url,
			})
		if not success then
			reject("Failed to create asset WebSocket client: " .. tostring(wsClient))
			return
		end
		self.__assetsWsClient = wsClient

		local closed, errored, received

		received = wsClient.MessageReceived:Connect(function(message)
			local packet = Http.msgpackDecode(message)
			if packet.sessionId ~= self.__sessionId then
				Log.warn("Received asset message with wrong session ID; ignoring")
				return
			end

			local ok, err = pcall(onPacket, packet)
			if not ok then
				Log.error("Error handling asset changes: {}", err)
			end
		end)

		closed = wsClient.Closed:Connect(function()
			closed:Disconnect()
			errored:Disconnect()
			received:Disconnect()

			if self.__connected and self.__assetsWsClient == wsClient then
				reject("Asset WebSocket connection closed unexpectedly")
			else
				resolve()
			end
		end)

		errored = wsClient.Error:Connect(function(code, message)
			closed:Disconnect()
			errored:Disconnect()
			received:Disconnect()

			reject("Asset WebSocket error: " .. code .. " - " .. message)
		end)
	end)
end

function ApiContext:disconnectAssetsWebSocket()
	local wsClient = self.__assetsWsClient
	self.__assetsWsClient = nil

	if wsClient then
		wsClient:Close()
	end
end

function ApiContext:refPatch(ids: { string })
	local url = ("%s/api/ref-patch"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return Http.post(url, request_body)
		:andThen(rejectFailedRequests)
		:andThen(Http.Response.msgpack)
		:andThen(function(response_body)
			if response_body.sessionId ~= self.__sessionId then
				return Promise.reject("Server changed ID")
			end

			assert(validateApiRefPatch(response_body))

			return response_body
		end)
end

return ApiContext
