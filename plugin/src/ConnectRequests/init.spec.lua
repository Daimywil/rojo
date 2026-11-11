return function()
	local HttpService = game:GetService("HttpService")

	local ConnectRequests = require(script.Parent)
	local protocol = ConnectRequests.protocol
	local Attribute = ConnectRequests.Attribute

	local SERVER_GONE = "WebSocket connection closed unexpectedly"
	local UNREACHABLE = "Couldn't connect to the Rojo server.\nMake sure the server is running — use 'rojo serve' to run it!"

	local function resume(thread, ...)
		local success, err = coroutine.resume(thread, ...)
		if not success then
			error(debug.traceback(thread, tostring(err)), 0)
		end
	end

	-- Attributes that tell their listeners about changes right away
	local function createAttributes()
		local values = {}
		local listeners = {}
		local attributes = {
			-- Every value an attribute had, in order
			history = {},
		}

		function attributes.get(name)
			return values[name]
		end

		function attributes.set(name, value)
			if values[name] == value then
				return
			end
			values[name] = value

			attributes.history[name] = attributes.history[name] or {}
			table.insert(attributes.history[name], value)

			for _, callback in listeners[name] or {} do
				callback()
			end
		end

		function attributes.onChanged(name, callback)
			listeners[name] = listeners[name] or {}
			table.insert(listeners[name], callback)

			return function()
				local index = table.find(listeners[name], callback)
				if index ~= nil then
					table.remove(listeners[name], index)
				end
			end
		end

		return attributes
	end

	-- Time that only passes when a test says so
	local function createScheduler()
		local time = 0
		local timers = {}
		local scheduler = {}

		function scheduler.clock()
			return time
		end

		function scheduler.now()
			return 1000000 + math.floor(time * 1000)
		end

		function scheduler.delay(seconds, callback)
			local timer = { at = time + seconds, callback = callback }
			table.insert(timers, timer)
			return timer
		end

		function scheduler.cancel(timer)
			timer.done = true
		end

		scheduler.resume = resume

		function scheduler.advance(seconds)
			local target = time + seconds

			while true do
				local due = nil
				for _, timer in timers do
					if not timer.done and timer.at <= target and (due == nil or timer.at < due.at) then
						due = timer
					end
				end
				if due == nil then
					break
				end

				time = due.at
				due.done = true
				due.callback()
			end

			time = target
		end

		return scheduler
	end

	local function projectSummary()
		return {
			scope = "project",
			counts = { added = 1, removed = 2, changed = 3, changedSource = 1 },
			paths = {
				added = { "ReplicatedStorage.New" },
				removed = { "Workspace.Old" },
				changed = { "ServerScriptService.Main" },
			},
		}
	end

	local function assetSummary(path: string)
		return {
			path = path,
			firstSync = false,
			toFs = 0,
			toStudio = 1,
			deleteFs = 0,
			deleteStudio = 0,
			upgrades = 0,
			conflicts = { { path = "Models/Tree.rbxm", reason = "bothChanged" } },
		}
	end

	--[[
		Stands in for the plugin and the server it connects to. Its sessions do
		what real ones do with ConnectRequests: report that they start, ask what
		to do when the files and Studio differ, and report how they end.
	]]
	local function createHost()
		local host = {
			panelPort = "34872",
			-- The server: whether it's there, and how its project differs from Studio
			reachable = true,
			failure = nil,
			projectName = "game",
			differs = false,
			assetPrompts = {},

			session = nil,
			-- Every session that was started, and what was decided in them
			connects = {},
			patchDecisions = {},
			assetDecisions = {},
			legacyRequests = {},
			waitingText = nil,
			-- What sessionEnded last said to show
			shown = nil,
		}

		function host.awaitReady() end

		function host.address(port)
			local text = if port ~= nil then tostring(port) else host.panelPort
			return "localhost:" .. text, text
		end

		function host.getSession()
			local session = host.session
			if session == nil then
				return nil
			end
			return { address = session.address, projectName = session.projectName, connected = session.connected }
		end

		function host.showWaiting(text)
			host.waitingText = text
		end

		function host.legacyConnect(value)
			table.insert(host.legacyRequests, value)
			return "Connecting to localhost:" .. tostring(value)
		end

		local function fail(session, details)
			if host.session == session then
				host.session = nil
				host.shown = host.controller:sessionEnded(details)
			end
		end

		-- Yields until the test answers the prompt that Studio shows
		local function askUser(session)
			host.controller:promptShown()
			session.prompting = true
			local decision = coroutine.yield()
			session.prompting = false
			return decision
		end

		function host.connect(port, quiet)
			local controller = host.controller
			local session = {
				address = "localhost:" .. port,
				port = port,
				connected = false,
			}
			host.session = session
			table.insert(host.connects, { port = port, quiet = quiet })

			session.thread = coroutine.create(function()
				controller:sessionStarting({ address = session.address, port = port })

				if not host.reachable then
					return fail(session, UNREACHABLE)
				elseif host.failure ~= nil then
					return fail(session, host.failure)
				end

				local decision = controller:decidePatch(projectSummary, not host.differs)
				if decision == nil then
					decision = askUser(session)
					controller:promptAnswered(decision)
				end
				if decision == "Abort" then
					return fail(session, "Aborted Rojo sync operation")
				end
				table.insert(host.patchDecisions, decision)

				for _, summary in host.assetPrompts do
					local assetDecision = controller:decideAssets(summary)
					if assetDecision == nil then
						assetDecision = askUser(session)
						controller:promptAnswered(assetDecision, summary)
					end
					if assetDecision == "Abort" then
						return fail(session, "AssetSync:800: Asset sync was aborted")
					end
					table.insert(host.assetDecisions, assetDecision)
				end

				if host.session ~= session then
					return
				end

				session.connected = true
				session.projectName = host.projectName
				controller:sessionConnected({
					address = session.address,
					port = port,
					projectName = host.projectName,
				})
			end)

			resume(session.thread)
		end

		function host.disconnect()
			local session = host.session
			host.controller:stopping()
			host.session = nil
			host.controller:stopped()

			-- The plugin lets go of whoever it was asking
			if session ~= nil and session.prompting then
				resume(session.thread, "Abort")
			end
		end

		-- The user answers the prompt that Studio shows
		function host.answer(decision)
			resume(host.session.thread, decision)
		end

		-- The server stops while a session is connected to it
		function host.loseServer()
			host.reachable = false
			fail(host.session, SERVER_GONE)
		end

		return host
	end

	local function setup()
		local attributes = createAttributes()
		local scheduler = createScheduler()
		local host = createHost()
		local ids = 0

		local controller = ConnectRequests.new({
			attributes = attributes,
			scheduler = scheduler,
			host = host,
			json = {
				encode = function(value)
					return HttpService:JSONEncode(value)
				end,
				decode = function(text)
					return HttpService:JSONDecode(text)
				end,
			},
			newId = function()
				ids += 1
				return "id-" .. ids
			end,
			studioSessionId = "studio-1",
		})
		host.controller = controller
		controller:start()

		local context = {
			attributes = attributes,
			scheduler = scheduler,
			host = host,
			controller = controller,
		}

		function context.request(fields)
			attributes.set(Attribute.Request, HttpService:JSONEncode(fields))
		end

		function context.result()
			local text = attributes.get(Attribute.Result)
			return if text == nil then nil else HttpService:JSONDecode(text)
		end

		function context.state()
			return HttpService:JSONDecode(attributes.get(Attribute.State))
		end

		function context.decide(decision)
			attributes.set(Attribute.Decision, decision)
		end

		-- The codes of every result so far, like "needs_decision connected"
		function context.resultCodes()
			local codes = {}
			for _, text in attributes.history[Attribute.Result] or {} do
				table.insert(codes, HttpService:JSONDecode(text).code)
			end
			return table.concat(codes, " ")
		end

		return context
	end

	describe("deciding between the files and Studio", function()
		it("should connect at once when they don't differ", function()
			local context = setup()
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })

			local result = context.result()
			expect(result.ok).to.equal(true)
			expect(result.code).to.equal("connected")
			expect(result.requestId).to.equal("r1")
			expect(result.address).to.equal("localhost:34873")
			expect(result.winner).to.equal(nil)
			expect(result.summary.scope).to.equal("project")

			-- Nothing was asked on the way
			expect(context.resultCodes()).to.equal("connected")
			expect(context.host.patchDecisions[1]).to.equal("Accept")
			expect(context.state().state).to.equal("connected")
			expect(context.state().projectName).to.equal("game")
		end)

		it("should apply nothing until a decision arrives", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })

			local result = context.result()
			expect(result.ok).to.equal(false)
			expect(result.code).to.equal("needs_decision")
			expect(result.requestId).to.equal("r1")
			expect(result.address).to.equal("localhost:34873")
			expect(result.winner).to.equal(nil)

			local summary = result.summary
			expect(summary.scope).to.equal("project")
			expect(summary.decisionId).to.equal("id-1")
			expect(summary.counts.added).to.equal(1)
			expect(summary.counts.removed).to.equal(2)
			expect(summary.counts.changed).to.equal(3)
			expect(summary.paths.added[1]).to.equal("ReplicatedStorage.New")
			expect(summary.paths.removed[1]).to.equal("Workspace.Old")
			expect(summary.paths.changed[1]).to.equal("ServerScriptService.Main")

			expect(#context.host.patchDecisions).to.equal(0)
			expect(context.state().state).to.equal("awaiting_decision")
			expect(context.state().awaiting).to.equal("caller")
			expect(context.state().requestId).to.equal("r1")
			expect(context.state().confirm).to.equal("decide")
			expect(type(context.host.waitingText)).to.equal("string")
		end)

		it("should make Studio match the files when the files win", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })
			context.decide("files")

			expect(context.host.patchDecisions[1]).to.equal("Accept")
			expect(context.attributes.get(Attribute.Decision)).to.equal(nil)
			expect(context.host.waitingText).to.equal(nil)

			local result = context.result()
			expect(result.ok).to.equal(true)
			expect(result.code).to.equal("connected")
			expect(result.winner).to.equal("files")
			expect(result.summary.counts.changed).to.equal(3)
			expect(result.decisions[1].scope).to.equal("project")
			expect(result.decisions[1].winner).to.equal("files")
			expect(result.decisions[1].by).to.equal("caller")
			expect(context.resultCodes()).to.equal("needs_decision connected")
			expect(context.state().state).to.equal("connected")
			expect(context.state().awaiting).to.equal(nil)
		end)

		it("should leave Studio as it is when Studio wins", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })
			context.decide("studio")

			-- Rejecting the patch is what Reject in the sync confirmation does
			expect(context.host.patchDecisions[1]).to.equal("Reject")

			local result = context.result()
			expect(result.ok).to.equal(true)
			expect(result.code).to.equal("connected")
			expect(result.winner).to.equal("studio")
			expect(context.state().state).to.equal("connected")
		end)

		it("should give up without changing anything when cancelled", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })
			context.decide("cancel")

			expect(#context.host.patchDecisions).to.equal(0)
			expect(context.host.session).to.equal(nil)
			-- Nothing went wrong, so the plugin doesn't show an error
			expect(context.host.shown).to.equal("quiet")

			local result = context.result()
			expect(result.ok).to.equal(false)
			expect(result.code).to.equal("cancelled")
			expect(result.requestId).to.equal("r1")
			expect(context.state().state).to.equal("disconnected")
			expect(context.state().lastError).to.equal(nil)
		end)

		it("should give up when no decision arrives for ten minutes", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })

			context.scheduler.advance(599)
			expect(context.result().code).to.equal("needs_decision")
			expect(context.state().state).to.equal("awaiting_decision")

			context.scheduler.advance(1)
			expect(#context.host.patchDecisions).to.equal(0)
			expect(context.host.session).to.equal(nil)

			local result = context.result()
			expect(result.ok).to.equal(false)
			expect(result.code).to.equal("decision_timeout")
			expect(context.state().state).to.equal("error")
			expect(context.state().lastError.code).to.equal("decision_timeout")

			-- A decision that comes too late changes nothing
			context.decide("files")
			expect(#context.host.patchDecisions).to.equal(0)
			expect(context.result().code).to.equal("decision_timeout")
		end)

		it("should keep waiting when the decision is not one", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide" })

			context.decide("maybe")
			context.decide(true)
			context.decide('{"decision": "files", "decisionId": "some-other-prompt"}')
			expect(context.result().code).to.equal("needs_decision")
			expect(#context.host.patchDecisions).to.equal(0)

			context.decide('{"decision": "files", "decisionId": "id-1"}')
			expect(context.result().code).to.equal("connected")
			expect(context.result().winner).to.equal("files")
		end)

		it("should ask about every asset directory with conflicts as well", function()
			local context = setup()
			context.host.differs = true
			context.host.assetPrompts = { assetSummary("assets/ServerStorage"), assetSummary("assets/Workspace") }
			context.request({ port = 34873, confirm = "decide" })
			context.decide("files")

			local result = context.result()
			expect(result.code).to.equal("needs_decision")
			expect(result.summary.scope).to.equal("assets")
			expect(result.summary.container).to.equal("assets/ServerStorage")
			expect(result.summary.decisionId).to.equal("id-2")
			expect(result.summary.counts.conflicts).to.equal(1)
			expect(result.summary.conflicts[1].path).to.equal("Models/Tree.rbxm")
			expect(context.state().state).to.equal("awaiting_decision")

			context.decide("studio")
			expect(context.result().summary.container).to.equal("assets/Workspace")
			context.decide("files")

			expect(context.host.assetDecisions[1]).to.equal("KeepStudio")
			expect(context.host.assetDecisions[2]).to.equal("KeepFiles")

			result = context.result()
			expect(result.code).to.equal("connected")
			-- The side that won for the project
			expect(result.winner).to.equal("files")
			expect(#result.decisions).to.equal(3)
			expect(result.decisions[2].scope).to.equal("assets")
			expect(result.decisions[2].container).to.equal("assets/ServerStorage")
			expect(result.decisions[2].winner).to.equal("studio")
			expect(result.decisions[3].winner).to.equal("files")
		end)

		it("should give up when an asset decision is cancelled", function()
			local context = setup()
			context.host.assetPrompts = { assetSummary("assets/ServerStorage") }
			context.request({ port = 34873, confirm = "decide" })
			expect(context.result().summary.scope).to.equal("assets")

			context.decide("cancel")
			expect(#context.host.assetDecisions).to.equal(0)
			expect(context.result().code).to.equal("cancelled")
			expect(context.state().state).to.equal("disconnected")
		end)
	end)

	describe("requests that say who wins", function()
		it("should let the files win without asking", function()
			local context = setup()
			context.host.differs = true
			context.host.assetPrompts = { assetSummary("assets/ServerStorage") }
			context.request({ port = 34873, confirm = "files" })

			expect(context.host.patchDecisions[1]).to.equal("Accept")
			expect(context.host.assetDecisions[1]).to.equal("KeepFiles")
			expect(context.resultCodes()).to.equal("connected")
			expect(context.result().winner).to.equal("files")
			expect(context.result().decisions[1].by).to.equal("request")
		end)

		it("should let Studio win without asking", function()
			local context = setup()
			context.host.differs = true
			context.host.assetPrompts = { assetSummary("assets/ServerStorage") }
			context.request({ port = 34873, confirm = "studio" })

			expect(context.host.patchDecisions[1]).to.equal("Reject")
			expect(context.host.assetDecisions[1]).to.equal("KeepStudio")
			expect(context.resultCodes()).to.equal("connected")
			expect(context.result().winner).to.equal("studio")
		end)

		it("should not have a winner when nothing differs", function()
			local context = setup()
			context.request({ port = 34873, confirm = "studio" })

			expect(context.host.patchDecisions[1]).to.equal("Accept")
			expect(context.result().winner).to.equal(nil)
			expect(context.result().decisions).to.equal(nil)
		end)

		it("should leave it to the user in Studio when asked to", function()
			local context = setup()
			context.host.differs = true
			context.host.assetPrompts = { assetSummary("assets/ServerStorage") }
			context.request({ port = 34873, confirm = "ask" })

			-- Nothing is asked of whoever made the request
			expect(context.result()).to.equal(nil)
			expect(context.state().state).to.equal("awaiting_decision")
			expect(context.state().awaiting).to.equal("studio")

			context.host.answer("Reject")
			expect(context.state().awaiting).to.equal("studio")
			context.host.answer("KeepFiles")

			local result = context.result()
			expect(result.code).to.equal("connected")
			expect(result.winner).to.equal("studio")
			expect(result.decisions[1].by).to.equal("studio")
			expect(result.decisions[2].winner).to.equal("files")
		end)

		it("should report that the user aborted in Studio as cancelled", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "ask" })
			context.host.answer("Abort")

			expect(context.result().code).to.equal("cancelled")
			expect(context.state().state).to.equal("disconnected")
		end)
	end)

	describe("requests", function()
		it("should answer true and ports the way they were always answered", function()
			local context = setup()

			context.attributes.set(Attribute.Request, 34873)
			expect(context.attributes.get(Attribute.Request)).to.equal(nil)
			expect(context.attributes.get(Attribute.Result)).to.equal("Connecting to localhost:34873")

			context.attributes.set(Attribute.Request, true)
			expect(context.attributes.get(Attribute.Result)).to.equal("Connecting to localhost:true")
			expect(#context.host.legacyRequests).to.equal(2)

			-- Their sessions are not started here, and are left to the plugin to decide about
			expect(#context.host.connects).to.equal(0)
			expect(context.controller:decidePatch(projectSummary, false)).to.equal(nil)
			expect(context.controller:decideAssets(assetSummary("assets"))).to.equal(nil)
		end)

		it("should use the port in the Rojo panel when none is given", function()
			local context = setup()
			context.request({ confirm = "files" })

			expect(context.host.connects[1].port).to.equal("34872")
			expect(context.result().address).to.equal("localhost:34872")
			expect(context.state().port).to.equal(34872)
		end)

		it("should refuse requests it can't carry out", function()
			local context = setup()
			context.request({ port = 34873, confirm = "maybe", requestId = "r1" })

			local result = context.result()
			expect(result.ok).to.equal(false)
			expect(result.code).to.equal("rejected")
			expect(result.requestId).to.equal("r1")
			expect(#context.host.connects).to.equal(0)
			expect(context.state().state).to.equal("disconnected")
		end)

		it("should say so when it's already connected to what is asked for", function()
			local context = setup()
			context.request({ port = 34873, confirm = "files", requestId = "r1" })
			context.request({ port = 34873, confirm = "decide", requestId = "r2" })

			local result = context.result()
			expect(result.ok).to.equal(true)
			expect(result.code).to.equal("already_connected")
			expect(result.requestId).to.equal("r2")
			expect(#context.host.connects).to.equal(1)

			-- The connection is the newer request's from here on
			expect(context.state().state).to.equal("connected")
			expect(context.state().requestId).to.equal("r2")
			expect(context.state().confirm).to.equal("decide")
		end)

		it("should not leave a connection to something else unless told to", function()
			local context = setup()
			context.request({ port = 34873, confirm = "files", requestId = "r1" })
			context.request({ port = 34999, confirm = "files", requestId = "r2" })

			local result = context.result()
			expect(result.ok).to.equal(false)
			expect(result.code).to.equal("already_connected")
			expect(result.address).to.equal("localhost:34873")
			expect(#context.host.connects).to.equal(1)
			expect(context.state().requestId).to.equal("r1")
		end)

		it("should disconnect first when told to replace the connection", function()
			local context = setup()
			context.request({ port = 34873, confirm = "files", requestId = "r1" })
			context.request({ port = 34999, confirm = "files", replace = true, requestId = "r2" })

			local result = context.result()
			expect(result.ok).to.equal(true)
			expect(result.code).to.equal("connected")
			expect(result.requestId).to.equal("r2")
			expect(result.address).to.equal("localhost:34999")
			expect(context.host.connects[2].port).to.equal("34999")
			expect(context.state().port).to.equal(34999)

			-- The connection it replaced doesn't get a result of its own
			expect(context.resultCodes()).to.equal("connected connected")
		end)

		it("should replace a connection that is waiting for a decision", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })
			context.request({ port = 34873, confirm = "files", replace = true, requestId = "r2" })

			expect(context.result().requestId).to.equal("r2")
			expect(context.result().code).to.equal("connected")
			expect(#context.host.patchDecisions).to.equal(1)

			-- The decision the first one waited for can't apply to anything anymore
			context.decide("studio")
			expect(#context.host.patchDecisions).to.equal(1)
		end)

		it("should report why a connection could not be made", function()
			local failures = {
				{ code = "version_mismatch", failure = protocol.codedError("version_mismatch", "Wrong version") },
				{ code = "place_not_served", failure = protocol.codedError("place_not_served", "Wrong place") },
				{ code = "sync_lock_held", failure = protocol.codedError("sync_lock_held", "Someone else syncs") },
				{ code = "error", failure = "Cannot sync a model as a place." },
			}

			for _, case in failures do
				local context = setup()
				context.host.failure = case.failure
				context.request({ port = 34873, confirm = "decide", requestId = "r1" })

				local result = context.result()
				expect(result.ok).to.equal(false)
				expect(result.code).to.equal(case.code)
				expect(result.message).to.equal(tostring(case.failure))
				expect(context.state().state).to.equal("error")
				expect(context.state().lastError.code).to.equal(case.code)
				expect(context.state().lastError.message).to.equal(tostring(case.failure))
				-- Nothing is tried again
				context.scheduler.advance(300)
				expect(#context.host.connects).to.equal(1)
			end
		end)

		it("should not wait for a server that was never there", function()
			local context = setup()
			context.host.reachable = false
			context.request({ port = 34873, confirm = "decide" })

			expect(context.result().code).to.equal("unreachable")
			expect(context.state().state).to.equal("error")
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(1)
		end)
	end)

	describe("the connection state", function()
		it("should start out disconnected", function()
			local context = setup()
			local state = context.state()

			expect(state.state).to.equal("disconnected")
			expect(state.port).to.equal(nil)
			expect(state.lastError).to.equal(nil)
			expect(state.studioSessionId).to.equal("studio-1")
			expect(state.handshake).to.equal(protocol.HANDSHAKE_VERSION)
		end)

		it("should say since when it's in the state it's in", function()
			local context = setup()
			context.host.differs = true
			expect(context.state().since).to.equal(1000000)

			context.scheduler.advance(2)
			context.request({ port = 34873, confirm = "decide" })
			expect(context.state().state).to.equal("awaiting_decision")
			expect(context.state().since).to.equal(1002000)

			context.scheduler.advance(3)
			context.decide("files")
			expect(context.state().state).to.equal("connected")
			expect(context.state().since).to.equal(1005000)
		end)

		it("should follow sessions that no request asked for", function()
			local context = setup()
			local controller = context.controller

			controller:sessionTakenOver()
			controller:sessionStarting({ address = "localhost:34872", port = "34872" })
			expect(context.state().state).to.equal("connecting")
			expect(context.state().port).to.equal(34872)
			expect(context.state().requestId).to.equal(nil)

			controller:promptShown()
			expect(context.state().state).to.equal("awaiting_decision")
			expect(context.state().awaiting).to.equal("studio")
			controller:promptAnswered("Accept")
			expect(context.state().state).to.equal("connecting")

			controller:sessionConnected({ address = "localhost:34872", port = "34872", projectName = "game" })
			expect(context.state().state).to.equal("connected")

			-- A session started by hand never reconnects by itself
			expect(controller:sessionEnded(SERVER_GONE)).to.equal(nil)
			expect(context.state().state).to.equal("error")
			expect(context.state().lastError.code).to.equal("unreachable")
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(0)

			-- And never writes a result
			expect(context.result()).to.equal(nil)
		end)
	end)

	describe("disconnecting", function()
		it("should disconnect when asked to", function()
			local context = setup()
			context.request({ port = 34873, confirm = "files" })

			context.attributes.set(Attribute.Disconnect, true)
			expect(context.attributes.get(Attribute.Disconnect)).to.equal(nil)
			expect(context.host.session).to.equal(nil)
			expect(context.state().state).to.equal("disconnected")
			expect(context.state().requestId).to.equal(nil)

			-- The answer to the request it had connected for stays what it was
			expect(context.result().code).to.equal("connected")
		end)

		it("should give up on a connection that waits for a decision", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "decide", requestId = "r1" })

			context.attributes.set(Attribute.Disconnect, "now")
			expect(#context.host.patchDecisions).to.equal(0)
			expect(context.result().code).to.equal("cancelled")
			expect(context.result().requestId).to.equal("r1")
			expect(context.state().state).to.equal("disconnected")
			expect(context.host.waitingText).to.equal(nil)

			context.scheduler.advance(1000)
			expect(context.result().code).to.equal("cancelled")
		end)

		it("should let go of the user when they were being asked", function()
			local context = setup()
			context.host.differs = true
			context.request({ port = 34873, confirm = "ask", requestId = "r1" })

			context.attributes.set(Attribute.Disconnect, true)
			expect(#context.host.patchDecisions).to.equal(0)
			expect(context.result().code).to.equal("cancelled")
			expect(context.state().state).to.equal("disconnected")
		end)

		it("should do nothing but say so when nothing is connected", function()
			local context = setup()
			context.attributes.set(Attribute.Disconnect, true)

			expect(context.attributes.get(Attribute.Disconnect)).to.equal(nil)
			expect(context.state().state).to.equal("disconnected")
			expect(context.result()).to.equal(nil)
		end)
	end)

	describe("reconnecting", function()
		local function connected(confirm: string?)
			local context = setup()
			context.request({ port = 34873, confirm = confirm or "decide", requestId = "r1" })
			expect(context.result().code).to.equal("connected")
			-- Long enough for the connection to count as one that worked
			context.scheduler.advance(60)
			return context
		end

		it("should try again with longer and longer waits when the server goes away", function()
			local context = connected()
			context.host.loseServer()

			-- The plugin shows that it's reconnecting rather than an error
			expect(context.host.shown).to.equal("reconnecting")
			expect(context.state().state).to.equal("reconnecting")
			expect(context.state().attempt).to.equal(1)
			expect(context.state().lastError.code).to.equal("unreachable")
			expect(context.state().requestId).to.equal("r1")
			expect(context.controller:isReconnecting()).to.equal(true)

			local attempts = {}
			local elapsed = 0
			for _ = 1, 400 do
				context.scheduler.advance(0.25)
				elapsed += 0.25
				if #context.host.connects - 1 > #attempts then
					table.insert(attempts, elapsed)
				end
				if #attempts == 7 then
					break
				end
			end

			expect(table.concat(attempts, " ")).to.equal("0.5 1.5 3.5 7.5 12.5 17.5 22.5")
			expect(context.state().state).to.equal("reconnecting")
			expect(context.state().attempt).to.equal(8)

			-- Every attempt is for the same port, and none of them announces itself
			for index = 2, #context.host.connects do
				expect(context.host.connects[index].port).to.equal("34873")
				expect(context.host.connects[index].quiet).to.equal(true)
			end

			-- The answer to the request stays what it was until there's a new one
			expect(context.resultCodes()).to.equal("connected")
		end)

		it("should connect again once the server is back", function()
			local context = connected()
			context.host.loseServer()
			context.scheduler.advance(10)

			context.host.reachable = true
			context.scheduler.advance(5)

			expect(context.state().state).to.equal("connected")
			expect(context.state().lastError).to.equal(nil)
			expect(context.state().attempt).to.equal(nil)
			expect(context.controller:isReconnecting()).to.equal(false)

			local result = context.result()
			expect(result.code).to.equal("connected")
			expect(result.requestId).to.equal("r1")
			expect(result.reconnected).to.equal(true)

			-- Nothing more is tried
			local connects = #context.host.connects
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(connects)
		end)

		it("should wait for a decision again when the files and Studio differ by then", function()
			local context = connected("decide")
			context.host.loseServer()
			context.host.reachable = true
			context.host.differs = true
			context.scheduler.advance(1)

			local result = context.result()
			expect(result.code).to.equal("needs_decision")
			expect(result.requestId).to.equal("r1")
			expect(context.state().state).to.equal("awaiting_decision")
			expect(#context.host.patchDecisions).to.equal(1)

			context.decide("studio")
			expect(context.host.patchDecisions[2]).to.equal("Reject")
			expect(context.result().code).to.equal("connected")
			expect(context.result().winner).to.equal("studio")
			expect(context.result().reconnected).to.equal(true)
		end)

		it("should use the confirm mode of the request when it reconnects", function()
			local context = connected("files")
			context.host.loseServer()
			context.host.reachable = true
			context.host.differs = true
			context.scheduler.advance(1)

			expect(context.host.patchDecisions[2]).to.equal("Accept")
			expect(context.result().code).to.equal("connected")
			expect(context.result().winner).to.equal("files")
		end)

		it("should give up after two minutes", function()
			local context = connected()
			context.host.loseServer()

			context.scheduler.advance(119)
			expect(context.state().state).to.equal("reconnecting")
			expect(context.resultCodes()).to.equal("connected")

			context.scheduler.advance(10)
			expect(context.state().state).to.equal("error")
			expect(context.state().lastError.code).to.equal("unreachable")
			expect(context.controller:isReconnecting()).to.equal(false)
			-- Now there is something to show
			expect(context.host.shown).to.equal(nil)

			local result = context.result()
			expect(result.ok).to.equal(false)
			expect(result.code).to.equal("unreachable")
			expect(result.requestId).to.equal("r1")

			local connects = #context.host.connects
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(connects)
		end)

		it("should not start over when the connection drops again right after reconnecting", function()
			local context = connected()
			context.host.loseServer()
			context.scheduler.advance(100)

			-- The server answers, but connections to it don't last
			for _ = 1, 20 do
				context.host.reachable = true
				context.scheduler.advance(5)
				if context.state().state ~= "connected" then
					break
				end
				context.host.loseServer()
			end

			expect(context.state().state).to.equal("error")
			expect(context.result().code).to.equal("unreachable")
		end)

		it("should stop when a disconnect is requested", function()
			local context = connected()
			context.host.loseServer()
			context.scheduler.advance(3)

			local connects = #context.host.connects
			context.attributes.set(Attribute.Disconnect, true)
			expect(context.state().state).to.equal("disconnected")
			expect(context.controller:isReconnecting()).to.equal(false)

			context.host.reachable = true
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(connects)
		end)

		it("should stop when the user disconnects", function()
			local context = connected()

			-- Disconnect in the Rojo panel
			context.host.disconnect()
			expect(context.state().state).to.equal("disconnected")

			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(1)
		end)

		it("should stop when the user connects by hand", function()
			local context = connected()
			context.host.loseServer()

			local connects = #context.host.connects
			context.controller:sessionTakenOver()
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(connects)
			expect(context.controller:isReconnecting()).to.equal(false)
		end)

		it("should make way for a new request", function()
			local context = connected()
			context.host.loseServer()
			context.host.reachable = true
			context.request({ port = 34999, confirm = "files", requestId = "r2" })

			expect(context.result().requestId).to.equal("r2")
			expect(context.result().code).to.equal("connected")
			expect(context.state().port).to.equal(34999)

			local connects = #context.host.connects
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(connects)
		end)

		it("should not reconnect when the request says not to", function()
			local context = setup()
			context.request({ port = 34873, confirm = "files", reconnect = false, requestId = "r1" })
			context.scheduler.advance(60)
			context.host.loseServer()

			expect(context.host.shown).to.equal(nil)
			expect(context.state().state).to.equal("error")
			expect(context.result().code).to.equal("unreachable")
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(1)
		end)

		it("should stop when reconnecting fails for another reason than the server being gone", function()
			local context = connected()
			context.host.loseServer()
			context.host.reachable = true
			context.host.failure = protocol.codedError("place_not_served", "Wrong place")
			context.scheduler.advance(1)

			expect(context.state().state).to.equal("error")
			expect(context.result().code).to.equal("place_not_served")

			local connects = #context.host.connects
			context.scheduler.advance(300)
			expect(#context.host.connects).to.equal(connects)
		end)
	end)
end
