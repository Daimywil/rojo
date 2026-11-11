--[[
	Lets other tools drive the plugin through attributes, by default those of
	CoreGui: a tool that can run Luau in Studio, like Studio's MCP, sets one
	to ask for something and reads another for the answer. Attributes are used
	because a thread with fewer capabilities than this plugin may not invoke a
	bindable of ours, and CoreGui because it's local to this Studio, so Team
	Create collaborators never see any of it.

	- RojoConnectRequest: set to ask for a connection. `true` or a port asks
	  for one the way that was always possible. A JSON object asks for one
	  whose prompts are answered by whoever asked: see protocol.parseRequest
	  for what it can say.
	- RojoConnectResult: what came of the last request. Plain text for `true`
	  and ports, JSON for JSON requests.
	- RojoConnectDecision: set to answer a result with the code
	  `needs_decision`, to "files", "studio" or "cancel".
	- RojoDisconnectRequest: set to anything to disconnect.
	- RojoConnectionState: what the plugin is doing, kept up to date as JSON
	  for every session, however it started.

	Everything this needs from the plugin and from Studio is passed in, so that
	what it does can be tested without either:

	- attributes: `get(name)`, `set(name, value)` and
	  `onChanged(name, callback)`, which returns a function that disconnects.
	- scheduler: `clock()` in seconds, `now()` in milliseconds since the Unix
	  epoch, `delay(seconds, callback)`, which returns something `cancel`
	  takes, and `resume(thread, ...)`.
	- json: `encode(value)` and `decode(text)`.
	- newId: returns a new unique string.
	- warn: logs a message.
	- studioSessionId: what this Studio calls itself to Rojo servers.
	- host: what sessions are made of:
	  - `awaitReady()` yields until the plugin is done starting up.
	  - `address(port: number?)` returns the address a port stands for and
	    the port as text, for the port in the Rojo panel without one.
	  - `getSession()` returns the address, project name and whether it's
	    connected of the session that's running, if one is.
	  - `connect(port: string, quiet: boolean)` starts a session for a
	    request without waiting for it. It then reports back through
	    `sessionStarting`, `sessionConnected` and `sessionEnded`, and asks
	    `decidePatch` and `decideAssets` what to do when the files and Studio
	    differ. A quiet one doesn't tell the user that it starts.
	  - `disconnect()` ends whatever is running, between calls to `stopping`
	    and `stopped`.
	  - `legacyConnect(value)` answers `true`, a port, and anything else that
	    isn't a JSON request, with plain text.
	  - `showWaiting(text: string?)` shows the user what a connection is
	    waiting for, or stops showing it.
]]

local protocol = require(script.protocol)

local Attribute = {
	Request = "RojoConnectRequest",
	Result = "RojoConnectResult",
	Decision = "RojoConnectDecision",
	Disconnect = "RojoDisconnectRequest",
	State = "RojoConnectionState",
}

local WAITING_FOR_DECISION = "Files and Studio differ.\n"
	.. "Waiting for the tool that asked for this connection to pick which to keep..."

local ConnectRequests = {}
ConnectRequests.__index = ConnectRequests

ConnectRequests.Attribute = Attribute
ConnectRequests.protocol = protocol

function ConnectRequests.new(deps)
	local self = {
		__attributes = deps.attributes,
		__scheduler = deps.scheduler,
		__json = deps.json,
		__host = deps.host,
		__newId = deps.newId,
		__warn = deps.warn or function() end,
		__studioSessionId = deps.studioSessionId,

		-- The connection a JSON request asked for, for as long as it's connecting, connected or reconnecting:
		-- the request, the port as text, the address, whether it ever connected, whether its request was
		-- answered for good, the decisions made for it, and a summary of how the project differed.
		__managed = nil,
		-- The prompt that waits for RojoConnectDecision, if one does
		__pending = nil,
		-- When reconnecting started and how often it was tried, while a connection waits to reconnect
		__reconnect = nil,
		-- The same for the last time it reconnected, in case that turns out not to have worked
		__lastReconnect = nil,
		__connectedAt = nil,
		-- Set while a session is ended on purpose, which is then not a failure
		__stopping = false,
		-- Set while a session is ended to make way for a request
		__replacing = false,
		-- Counts requests, to tell when a newer one arrived while one was waiting
		__requestCount = 0,

		__state = nil,
		__connections = {},
	}

	return setmetatable(self, ConnectRequests)
end

--[[
	Starts listening for requests.
]]
function ConnectRequests:start()
	local attributes = self.__attributes

	-- Whatever was asked of a plugin that isn't there anymore is not asked of this one
	attributes.set(Attribute.Request, nil)
	attributes.set(Attribute.Decision, nil)
	attributes.set(Attribute.Disconnect, nil)
	-- A session can have started before this, like the one that connects a playtest by itself
	if self.__state == nil then
		self:__setState("disconnected", {})
	end

	-- Every request is cleared once it's read, so that asking for the same thing again still changes the attribute
	table.insert(
		self.__connections,
		attributes.onChanged(Attribute.Request, function()
			local value = attributes.get(Attribute.Request)
			if value ~= nil then
				attributes.set(Attribute.Request, nil)
				self:__handleRequest(value)
			end
		end)
	)
	table.insert(
		self.__connections,
		attributes.onChanged(Attribute.Decision, function()
			local value = attributes.get(Attribute.Decision)
			if value ~= nil then
				attributes.set(Attribute.Decision, nil)
				self:__handleDecision(value)
			end
		end)
	)
	table.insert(
		self.__connections,
		attributes.onChanged(Attribute.Disconnect, function()
			if attributes.get(Attribute.Disconnect) ~= nil then
				attributes.set(Attribute.Disconnect, nil)
				self.__host.disconnect()
			end
		end)
	)
end

function ConnectRequests:destroy()
	for _, disconnect in self.__connections do
		disconnect()
	end
	table.clear(self.__connections)

	self:__cancelReconnect()
	local pending = self.__pending
	if pending ~= nil then
		self:__resolve(pending, "stopped")
	end
end

--[[
	Whether a connection that a request asked for waits for something the
	user may want to call off: a decision or the server coming back.
]]
function ConnectRequests:isWaiting(): boolean
	return self.__pending ~= nil or self.__reconnect ~= nil
end

function ConnectRequests:isReconnecting(): boolean
	return self.__reconnect ~= nil
end

function ConnectRequests:__writeResult(result)
	self.__attributes.set(Attribute.Result, protocol.encodeResult(result, self.__json.encode))
end

--[[
	Publishes what the plugin is doing. `changes` has what's different from
	before: `port`, `address`, `projectName` and `lastError` stay as they are
	until they're given again, with false to clear them, while `awaiting` and
	`attempt` only last for the state they're given with.
]]
function ConnectRequests:__setState(state: string, changes)
	local previous = self.__state or {}
	local managed = self.__managed

	local function kept(name: string)
		local value = changes[name]
		if value == nil then
			return previous[name]
		elseif value == false then
			return nil
		end
		return value
	end

	local new = {
		state = state,
		port = kept("port"),
		-- When this state began
		since = if previous.state == state then previous.since else self.__scheduler.now(),
		lastError = kept("lastError"),
		address = kept("address"),
		projectName = kept("projectName"),
		requestId = if managed ~= nil then managed.request.requestId else nil,
		confirm = if managed ~= nil then managed.request.confirm else nil,
		awaiting = changes.awaiting,
		attempt = changes.attempt,
		studioSessionId = self.__studioSessionId,
		handshake = protocol.HANDSHAKE_VERSION,
	}

	self.__state = new
	self.__attributes.set(Attribute.State, protocol.encodeState(new, self.__json.encode))
end

function ConnectRequests:__error(code: string, message: string)
	return { code = code, message = message, time = self.__scheduler.now() }
end

function ConnectRequests:__cancelReconnect()
	local reconnect = self.__reconnect
	self.__reconnect = nil

	if reconnect ~= nil and reconnect.timer ~= nil then
		self.__scheduler.cancel(reconnect.timer)
		reconnect.timer = nil
	end
end

function ConnectRequests:__handleRequest(value: any)
	local host = self.__host
	local parsed = protocol.parseRequest(value, self.__json.decode)

	if parsed.kind == "legacy" then
		self.__attributes.set(Attribute.Result, host.legacyConnect(value))
		return
	end

	if parsed.kind == "invalid" then
		self:__writeResult({
			requestId = parsed.requestId,
			ok = false,
			code = protocol.Code.rejected,
			message = parsed.message,
		})
		return
	end

	local request = parsed.request

	self.__requestCount += 1
	local requestCount = self.__requestCount
	host.awaitReady()
	if requestCount ~= self.__requestCount then
		-- A newer request arrived in the meantime, and the result is for that one
		return
	end

	local address, port = host.address(request.port)

	local session = host.getSession()
	if session ~= nil then
		if session.address == address and session.connected then
			-- It already is what was asked for. However it got connected, it's the request's connection from here
			-- on, so that it reconnects the way the request wants.
			self:__cancelReconnect()
			self.__managed = {
				request = request,
				port = port,
				address = address,
				everConnected = true,
				answered = true,
				decisions = {},
			}
			self:__writeResult({
				requestId = request.requestId,
				ok = true,
				code = protocol.Code.already_connected,
				address = address,
				message = string.format("Already connected to '%s' at %s", tostring(session.projectName), address),
			})
			self:__setState("connected", {})
			return
		end

		if not request.replace then
			self:__writeResult({
				requestId = request.requestId,
				ok = false,
				code = protocol.Code.already_connected,
				address = session.address,
				message = string.format(
					"Already %s %s. Set replace to true to connect to %s instead.",
					if session.connected then "connected to" else "connecting to",
					session.address,
					address
				),
			})
			return
		end

		self.__replacing = true
		host.disconnect()
		self.__replacing = false
	end

	-- A connection that was still waiting to reconnect makes way too
	self:__cancelReconnect()
	self.__lastReconnect = nil

	self.__managed = {
		request = request,
		port = port,
		address = address,
		everConnected = false,
		answered = false,
		decisions = {},
	}
	self:__setState("connecting", {
		port = tonumber(port) or false,
		address = address,
		projectName = false,
		lastError = false,
	})
	host.connect(port, false)
end

function ConnectRequests:__handleDecision(value: any)
	local pending = self.__pending
	if pending == nil then
		self.__warn("Ignoring RojoConnectDecision because no connection is waiting for a decision")
		return
	end

	local decision, decisionId = protocol.parseDecision(value, self.__json.decode)
	if decision == nil then
		self.__warn(
			string.format('Ignoring RojoConnectDecision because %s is not "files", "studio" or "cancel"', tostring(value))
		)
		return
	end

	if decisionId ~= nil and decisionId ~= pending.id then
		self.__warn("Ignoring RojoConnectDecision because it answers a prompt that is no longer open")
		return
	end

	self:__resolve(pending, decision)
end

function ConnectRequests:__resolve(pending, outcome: string)
	if self.__pending ~= pending then
		return
	end
	self.__pending = nil

	if pending.timer ~= nil then
		self.__scheduler.cancel(pending.timer)
		pending.timer = nil
	end

	self.__scheduler.resume(pending.thread, outcome)
end

--[[
	Asks whoever made the request to pick a side, and yields until they do.
	Returns "files" or "studio", or nil if the connection should be given up.
]]
function ConnectRequests:__awaitDecision(managed, summary): string?
	local scheduler = self.__scheduler

	-- Lets an answer say which prompt it's for, since a connection can ask more than once
	summary.decisionId = self.__newId()
	-- An answer to an earlier prompt must not answer this one
	self.__attributes.set(Attribute.Decision, nil)

	local pending = {
		id = summary.decisionId,
		thread = coroutine.running(),
	}
	pending.timer = scheduler.delay(protocol.DECISION_TIMEOUT, function()
		pending.timer = nil
		self:__resolve(pending, "timeout")
	end)
	self.__pending = pending

	managed.answered = false
	self:__setState("awaiting_decision", { awaiting = "caller" })
	self.__host.showWaiting(WAITING_FOR_DECISION)
	self:__writeResult({
		requestId = managed.request.requestId,
		ok = false,
		code = protocol.Code.needs_decision,
		address = managed.address,
		message = 'Files and Studio differ. Set RojoConnectDecision to "files", "studio" or "cancel".',
		summary = summary,
	})

	local outcome = coroutine.yield()

	if outcome == "files" or outcome == "studio" then
		self.__host.showWaiting(nil)
		self:__setState("connecting", {})
		return outcome
	end

	-- The session reports that it was aborted, which doesn't say why
	if outcome == "cancel" then
		managed.abort = protocol.codedError(protocol.Code.cancelled, "The connection was cancelled")
	elseif outcome == "timeout" then
		managed.abort = protocol.codedError(
			protocol.Code.decision_timeout,
			string.format("No decision arrived within %d minutes", protocol.DECISION_TIMEOUT / 60)
		)
	end

	return nil
end

local function recordDecision(managed, summary, winner: string, by: string)
	table.insert(managed.decisions, {
		scope = summary.scope,
		container = summary.container,
		winner = winner,
		-- "request" if the request said who wins, "caller" if it was asked, "studio" if the user picked in Studio
		by = by,
	})
end

--[[
	Decides what a session does with the patch that makes Studio match the
	files, for the session's confirm callback. `getSummary` returns
	protocol.summarizePatch of it, and `trivial` is whether applying it
	changes nothing worth asking about.

	Returns "Accept", "Reject" or "Abort", or nil if the session is not for
	a request that settles this, in which case the plugin decides like it
	does for a session started by hand.
]]
function ConnectRequests:decidePatch(getSummary: () -> any, trivial: boolean): string?
	local managed = self.__managed
	if managed == nil then
		return nil
	end

	local summary = getSummary()
	managed.summary = summary
	managed.trivial = trivial

	if trivial then
		return "Accept"
	end

	local confirm = managed.request.confirm
	if confirm == "ask" then
		return nil
	elseif confirm == "files" then
		recordDecision(managed, summary, "files", "request")
		return "Accept"
	elseif confirm == "studio" then
		recordDecision(managed, summary, "studio", "request")
		return "Reject"
	end

	local winner = self:__awaitDecision(managed, summary)
	if winner == nil then
		return "Abort"
	end

	recordDecision(managed, summary, winner, "caller")
	return if winner == "files" then "Accept" else "Reject"
end

--[[
	Decides what asset sync does about an asset directory it would otherwise
	ask the user about (see AssetSync.Summary).

	Returns "KeepStudio", "KeepFiles" or "Abort", or nil if the plugin should
	ask the user.
]]
function ConnectRequests:decideAssets(assetSummary): string?
	local managed = self.__managed
	if managed == nil then
		return nil
	end

	local confirm = managed.request.confirm
	if confirm == "ask" then
		return nil
	end

	local summary = protocol.summarizeAssets(assetSummary)

	if confirm == "files" then
		recordDecision(managed, summary, "files", "request")
		return "KeepFiles"
	elseif confirm == "studio" then
		recordDecision(managed, summary, "studio", "request")
		return "KeepStudio"
	end

	local winner = self:__awaitDecision(managed, summary)
	if winner == nil then
		return "Abort"
	end

	recordDecision(managed, summary, winner, "caller")
	return if winner == "files" then "KeepFiles" else "KeepStudio"
end

--[[
	Called when the plugin starts asking the user to pick a side.
]]
function ConnectRequests:promptShown()
	self:__setState("awaiting_decision", { awaiting = "studio" })
end

--[[
	Called with what the plugin decided by itself or by asking the user,
	which is one of the answers that decidePatch and decideAssets give.
	`assetSummary` is given for decisions about an asset directory.
]]
function ConnectRequests:promptAnswered(decision: string, assetSummary: any?)
	if self.__state ~= nil and self.__state.state == "awaiting_decision" and decision ~= "Abort" then
		self:__setState("connecting", {})
	end

	local managed = self.__managed
	if managed == nil then
		return
	end

	if assetSummary ~= nil then
		if decision == "KeepFiles" or decision == "KeepStudio" then
			local summary = protocol.summarizeAssets(assetSummary)
			recordDecision(managed, summary, if decision == "KeepFiles" then "files" else "studio", "studio")
		end
	elseif managed.summary ~= nil and not managed.trivial then
		if decision == "Accept" or decision == "Reject" then
			recordDecision(managed, managed.summary, if decision == "Accept" then "files" else "studio", "studio")
		end
	end
end

--[[
	Called when a session starts connecting, with its `address` and `port`.
]]
function ConnectRequests:sessionStarting(info)
	if self.__reconnect ~= nil then
		-- Still reconnecting, until it's connected again
		return
	end

	self:__setState("connecting", {
		port = tonumber(info.port) or false,
		address = info.address,
		projectName = false,
		lastError = false,
	})
end

--[[
	Called when a session is connected, with its `address`, `port` and
	`projectName`.
]]
function ConnectRequests:sessionConnected(info)
	local reconnected = self.__reconnect ~= nil
	self.__lastReconnect = self.__reconnect
	self:__cancelReconnect()
	self.__connectedAt = self.__scheduler.clock()

	local managed = self.__managed
	if managed ~= nil then
		managed.everConnected = true
		managed.answered = true
		managed.address = info.address
	end

	self:__setState("connected", {
		port = tonumber(info.port) or false,
		address = info.address,
		projectName = info.projectName,
		lastError = false,
	})

	if managed == nil then
		return
	end

	-- The side that won for the project if that had to be decided, and else for the assets
	local first = managed.decisions[1]

	self:__writeResult({
		requestId = managed.request.requestId,
		ok = true,
		code = protocol.Code.connected,
		address = info.address,
		message = string.format("Connected to '%s' at %s", tostring(info.projectName), info.address),
		winner = if first ~= nil then first.winner else nil,
		summary = managed.summary,
		decisions = if #managed.decisions > 0 then managed.decisions else nil,
		reconnected = if reconnected then true else nil,
	})

	-- The next time it connects starts over
	managed.decisions = {}
	managed.summary = nil
end

--[[
	Waits to try connecting again after the server went away. Returns false
	once it has been trying for too long.
]]
function ConnectRequests:__scheduleReconnect(managed, lastError): boolean
	local scheduler = self.__scheduler
	local now = scheduler.clock()

	local reconnect = self.__reconnect
	if reconnect == nil then
		local last = self.__lastReconnect
		if
			last ~= nil
			and self.__connectedAt ~= nil
			and now - self.__connectedAt < protocol.RECONNECT_STABLE_AFTER
		then
			-- It dropped right after reconnecting, so it carries on where it was rather than trying forever
			reconnect = last
		else
			reconnect = { startedAt = now, attempt = 0 }
		end
	end
	self.__lastReconnect = nil

	local elapsed = now - reconnect.startedAt
	if elapsed >= protocol.RECONNECT_WINDOW then
		self.__reconnect = nil
		return false
	end

	reconnect.attempt += 1
	self.__reconnect = reconnect
	self:__setState("reconnecting", { attempt = reconnect.attempt, lastError = lastError })

	local delay = math.min(protocol.reconnectDelay(reconnect.attempt), protocol.RECONNECT_WINDOW - elapsed)
	reconnect.timer = scheduler.delay(delay, function()
		reconnect.timer = nil
		if self.__reconnect == reconnect and self.__managed == managed then
			self.__host.connect(managed.port, true)
		end
	end)

	return true
end

--[[
	Called when a session ended, or could not start, with the error that
	ended it if one did.

	Returns what the plugin should show for it: "reconnecting" if it will be
	tried again, "quiet" if it ended because that was asked for, and nil for
	whatever it shows when a session started by hand ends like this.
]]
function ConnectRequests:sessionEnded(details: any): string?
	if self.__stopping then
		-- `stopped` follows
		return nil
	end

	local managed = self.__managed

	if details == nil then
		self:__cancelReconnect()
		self.__managed = nil
		self:__setState("disconnected", {})
		return nil
	end

	local code, message = protocol.classifyError(details)
	if managed ~= nil and managed.abort ~= nil then
		code, message = protocol.classifyError(managed.abort)
		managed.abort = nil
	end

	if managed == nil then
		if code == protocol.Code.cancelled then
			self:__setState("disconnected", {})
		else
			self:__setState("error", { lastError = self:__error(code, message) })
		end
		return nil
	end

	-- Only a connection that worked before is worth waiting for: its server is most likely restarting
	if code == protocol.Code.unreachable and managed.everConnected and managed.request.reconnect then
		if self:__scheduleReconnect(managed, self:__error(code, message)) then
			return "reconnecting"
		end

		message = string.format(
			"Gave up reconnecting after %d minutes. %s",
			protocol.RECONNECT_WINDOW / 60,
			message
		)
	end

	self:__cancelReconnect()
	self.__lastReconnect = nil
	self.__managed = nil

	self:__writeResult({
		requestId = managed.request.requestId,
		ok = false,
		code = code,
		address = managed.address,
		message = message,
	})

	if code == protocol.Code.cancelled then
		self:__setState("disconnected", {})
		return "quiet"
	end

	self:__setState("error", { lastError = self:__error(code, message) })
	return nil
end

--[[
	Called before the plugin ends a session on purpose, or gives up on one
	that waits to reconnect: when the user disconnects, when a disconnect is
	requested, and when a request replaces a connection.
]]
function ConnectRequests:stopping()
	self.__stopping = true
	self:__cancelReconnect()
end

--[[
	Called once the session that `stopping` announced has ended.
]]
function ConnectRequests:stopped()
	self.__stopping = false
	self.__lastReconnect = nil

	local managed = self.__managed
	self.__managed = nil

	-- A request that was still waiting for its connection never gets it now. A request that replaces it writes
	-- its own result.
	if managed ~= nil and not managed.answered and not self.__replacing then
		self:__writeResult({
			requestId = managed.request.requestId,
			ok = false,
			code = protocol.Code.cancelled,
			address = managed.address,
			message = "Disconnected before the connection was made",
		})
	end

	self:__setState("disconnected", {})
	self.__host.showWaiting(nil)

	-- Whatever waited for a decision gives up, now that its session can't act on one anymore
	local pending = self.__pending
	if pending ~= nil then
		self:__resolve(pending, "stopped")
	end
end

--[[
	Called when a session starts that no request asked for, like one the user
	starts. A connection that was waiting to reconnect then no longer does.
]]
function ConnectRequests:sessionTakenOver()
	self:__cancelReconnect()
	self.__lastReconnect = nil
	self.__managed = nil
end

return ConnectRequests
