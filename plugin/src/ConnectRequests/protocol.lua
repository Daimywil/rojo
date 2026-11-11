--[[
	The formats of the attributes that other tools drive the plugin through
	(see ConnectRequests), kept apart from what acts on them so that they can
	be tested on their own. Nothing here touches the game.
]]

local protocol = {}

-- How many instance paths of each kind a summary lists at most
protocol.MAX_PATHS = 50
-- How long a connection waits for whoever asked for it to pick a side, in seconds
protocol.DECISION_TIMEOUT = 600
-- How long a connection that lost its server keeps trying to reconnect, in seconds
protocol.RECONNECT_WINDOW = 120
-- How long it waits before trying again: this long at first, twice as long every time after, up to the longest
protocol.RECONNECT_FIRST_DELAY = 0.5
protocol.RECONNECT_MAX_DELAY = 5
-- A connection that drops again this soon after reconnecting didn't really reconnect, so it doesn't get a new window
protocol.RECONNECT_STABLE_AFTER = 10
-- Tools can tell from this which of these formats a plugin understands
protocol.HANDSHAKE_VERSION = 2

-- Every code a result can have
protocol.Code = {
	connected = "connected",
	needs_decision = "needs_decision",
	cancelled = "cancelled",
	decision_timeout = "decision_timeout",
	already_connected = "already_connected",
	unreachable = "unreachable",
	version_mismatch = "version_mismatch",
	place_not_served = "place_not_served",
	sync_lock_held = "sync_lock_held",
	rejected = "rejected",
	error = "error",
}

local CONFIRM_MODES = {
	decide = true,
	files = true,
	studio = true,
	ask = true,
}

local DECISIONS = {
	files = true,
	studio = true,
	cancel = true,
}

local function isPort(value: any): boolean
	return type(value) == "number" and value % 1 == 0 and value >= 1 and value <= 65535
end

--[[
	An error that says which code it is reported with, for errors that
	can't be told apart by their message. It still reads as its message
	wherever errors are shown.
]]
local CodedError = {}
CodedError.__index = CodedError
CodedError.__tostring = function(self)
	return self.message
end

function protocol.codedError(code: string, message: string)
	return setmetatable({ code = code, message = message }, CodedError)
end

--[[
	Tells what kind of connect request a value of the request attribute is:

	- `{ kind = "legacy" }` for `true`, a bare port, and anything else the
	  plugin answered with plain text before requests could be JSON.
	- `{ kind = "request", request = ... }` for a valid JSON request.
	- `{ kind = "invalid", message = ..., requestId = ... }` for a JSON
	  request that can't be carried out as written.
]]
function protocol.parseRequest(value: any, jsonDecode: (string) -> any)
	if type(value) ~= "string" then
		return { kind = "legacy" }
	end

	local decoded, object = pcall(jsonDecode, value)
	if not decoded or type(object) ~= "table" then
		-- Text that was clearly meant as a JSON request gets an answer in kind
		if string.match(value, "^%s*{") then
			return { kind = "invalid", message = "The request is not valid JSON" }
		end
		return { kind = "legacy" }
	end

	local requestId = object.requestId
	if type(requestId) == "number" then
		requestId = tostring(requestId)
	elseif type(requestId) ~= "string" then
		requestId = nil
	end

	local function invalid(message: string)
		return { kind = "invalid", message = message, requestId = requestId }
	end

	if object.requestId ~= nil and requestId == nil then
		return invalid("requestId has to be a string")
	end

	-- Without a port, the request is for the one in the Rojo panel, like `true` is
	local port = object.port
	if port == true then
		port = nil
	elseif port ~= nil and not isPort(port) then
		return invalid("port has to be a whole number from 1 to 65535")
	end

	-- A request that doesn't say who wins must never overwrite anything by itself
	local confirm = object.confirm
	if confirm == nil then
		confirm = "decide"
	elseif type(confirm) ~= "string" or not CONFIRM_MODES[confirm] then
		return invalid('confirm has to be "decide", "files", "studio" or "ask"')
	end

	for _, name in { "replace", "reconnect" } do
		if object[name] ~= nil and type(object[name]) ~= "boolean" then
			return invalid(name .. " has to be true or false")
		end
	end

	return {
		kind = "request",
		request = {
			port = port,
			confirm = confirm,
			replace = object.replace == true,
			reconnect = object.reconnect ~= false,
			requestId = requestId,
		},
	}
end

--[[
	Reads a value of the decision attribute: "files", "studio" or "cancel",
	or a JSON object with that as `decision` and optionally the `decisionId`
	of the summary it answers. Returns the decision and the ID, or nil if
	it's neither.
]]
function protocol.parseDecision(value: any, jsonDecode: (string) -> any): (string?, string?)
	if type(value) ~= "string" then
		return nil, nil
	end

	if DECISIONS[value] then
		return value, nil
	end

	local decoded, object = pcall(jsonDecode, value)
	if not decoded or type(object) ~= "table" then
		return nil, nil
	end

	if type(object.decision) ~= "string" or not DECISIONS[object.decision] then
		return nil, nil
	end

	return object.decision, if type(object.decisionId) == "string" then object.decisionId else nil
end

--[[
	Picks the code that a failed connection is reported with, and returns it
	with the error as text.
]]
function protocol.classifyError(err: any): (string, string)
	local message = tostring(err)

	if type(err) == "table" then
		if type(err.code) == "string" and protocol.Code[err.code] ~= nil then
			return err.code, message
		end
		if err.message ~= nil then
			message = tostring(err.message)
		end
	end

	-- Most errors only keep their text on the way here, so that's what tells them apart
	local function has(text: string): boolean
		return string.find(message, text, 1, true) ~= nil
	end

	-- A server that stopped, hasn't started yet, or was replaced by another one
	if
		has("Couldn't connect to the Rojo server")
		or has("HTTP request timed out")
		or has("WebSocket")
		or has("Server changed ID")
	then
		return protocol.Code.unreachable, message
	end

	if has("Aborted Rojo sync operation") or has("Asset sync was aborted") then
		return protocol.Code.cancelled, message
	end

	return protocol.Code.error, message
end

--[[
	How long to wait before the given attempt to reconnect, counted from 1.
]]
function protocol.reconnectDelay(attempt: number): number
	return math.min(protocol.RECONNECT_FIRST_DELAY * 2 ^ (attempt - 1), protocol.RECONNECT_MAX_DELAY)
end

local function instancePath(instance, root): string
	if instance == root then
		return "game"
	end
	return instance:GetFullName()
end

-- Whether an update only renames the DataModel, which Studio names after the place whatever a project calls it
local function isRootRename(update, instanceMap, root): boolean
	return instanceMap.fromIds[update.id] == root
		and update.changedClassName == nil
		and next(update.changedProperties) == nil
end

--[[
	Whether applying a patch changes nothing worth asking about: it's empty,
	or only renames the DataModel.
]]
function protocol.isTrivialPatch(patch, instanceMap, root): boolean
	if next(patch.added) ~= nil or next(patch.removed) ~= nil then
		return false
	end

	for _, update in patch.updated do
		if not isRootRename(update, instanceMap, root) then
			return false
		end
	end

	return true
end

local function sortedAndCapped(paths: { string }): { string }
	table.sort(paths)
	return table.move(paths, 1, math.min(#paths, protocol.MAX_PATHS), 1, {})
end

--[[
	Describes the patch that would make Studio match the files, for whoever
	has to pick a side: how many instances the files would add to Studio,
	remove from it, and change in it, and where some of them are.

	Counts are of instances, with every descendant counted. Paths are those of
	the topmost instance of everything that is added or removed.
]]
function protocol.summarizePatch(patch, instanceMap, root)
	local added, addedPaths = 0, {}
	for _, virtualInstance in patch.added do
		added += 1

		-- Only the top of what's added has a parent that exists already
		if patch.added[virtualInstance.Parent] == nil then
			local parent = instanceMap.fromIds[virtualInstance.Parent]
			if parent == nil or parent == root then
				table.insert(addedPaths, virtualInstance.Name)
			else
				table.insert(addedPaths, parent:GetFullName() .. "." .. virtualInstance.Name)
			end
		end
	end

	local removed, removedPaths = 0, {}
	for _, idOrInstance in patch.removed do
		local instance = if type(idOrInstance) == "string" then instanceMap.fromIds[idOrInstance] else idOrInstance
		if instance == nil then
			removed += 1
			continue
		end

		removed += 1 + #instance:GetDescendants()
		table.insert(removedPaths, instancePath(instance, root))
	end

	local changed, changedSource, changedPaths = 0, 0, {}
	for _, update in patch.updated do
		if isRootRename(update, instanceMap, root) then
			continue
		end

		changed += 1
		if update.changedProperties.Source ~= nil then
			changedSource += 1
		end

		local instance = instanceMap.fromIds[update.id]
		if instance ~= nil then
			table.insert(changedPaths, instancePath(instance, root))
		end
	end

	return {
		scope = "project",
		counts = {
			added = added,
			removed = removed,
			changed = changed,
			-- Scripts whose source differs are all that "studio" writes to files
			changedSource = changedSource,
		},
		paths = {
			added = sortedAndCapped(addedPaths),
			removed = sortedAndCapped(removedPaths),
			changed = sortedAndCapped(changedPaths),
		},
	}
end

--[[
	Describes what asset sync asks about for one asset directory (see
	AssetSync.Summary), for whoever has to pick a side.
]]
function protocol.summarizeAssets(summary)
	local conflicts = {}
	for index, conflict in summary.conflicts do
		if index > protocol.MAX_PATHS then
			break
		end
		conflicts[index] = { path = conflict.path, reason = conflict.reason }
	end

	return {
		scope = "assets",
		container = summary.path,
		firstSync = summary.firstSync == true,
		counts = {
			conflicts = #summary.conflicts,
			-- What syncing does besides settling the conflicts if Studio wins. If the files win, Studio is made to
			-- match them for every asset and nothing on disk changes.
			toFiles = summary.toFs + (summary.upgrades or 0),
			deleteFiles = summary.deleteFs,
			toStudio = summary.toStudio,
			deleteStudio = summary.deleteStudio,
		},
		conflicts = conflicts,
	}
end

--[[
	Encodes an object with its fields in the given order. Fields without a
	value are written as null, which JSONEncode has no way to do, so that
	whoever reads the object always finds the same fields in it.
]]
local function encodeObject(keys: { string }, values: { [string]: any }, jsonEncode: (any) -> string): string
	local fields = table.create(#keys)
	for index, key in keys do
		local value = values[key]
		fields[index] = string.format("%s:%s", jsonEncode(key), if value == nil then "null" else jsonEncode(value))
	end

	return "{" .. table.concat(fields, ",") .. "}"
end

local RESULT_KEYS = { "requestId", "ok", "address", "code", "message", "winner", "summary" }
-- Only part of the results that have them
local OPTIONAL_RESULT_KEYS = { "decisions", "reconnected" }

function protocol.encodeResult(result, jsonEncode: (any) -> string): string
	local keys = table.clone(RESULT_KEYS)
	for _, key in OPTIONAL_RESULT_KEYS do
		if result[key] ~= nil then
			table.insert(keys, key)
		end
	end

	return encodeObject(keys, result, jsonEncode)
end

local STATE_KEYS = {
	"state",
	"port",
	"since",
	"lastError",
	"address",
	"projectName",
	"requestId",
	"confirm",
	"awaiting",
	"attempt",
	"studioSessionId",
	"handshake",
}

function protocol.encodeState(state, jsonEncode: (any) -> string): string
	return encodeObject(STATE_KEYS, state, jsonEncode)
end

return protocol
