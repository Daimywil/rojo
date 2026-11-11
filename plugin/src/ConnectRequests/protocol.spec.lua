return function()
	local HttpService = game:GetService("HttpService")

	local protocol = require(script.Parent.protocol)

	local function decode(text: string)
		return HttpService:JSONDecode(text)
	end

	local function encode(value: any): string
		return HttpService:JSONEncode(value)
	end

	-- Stands in for an instance, which is all that summaries ask of one
	local function instance(fullName: string, descendants: number?)
		return {
			GetFullName = function()
				return fullName
			end,
			GetDescendants = function()
				return table.create(descendants or 0, true)
			end,
		}
	end

	describe("parseRequest", function()
		it("should leave true, ports and other text to the plain text answers", function()
			for _, value in { true, 34872, 1.5, "34872", "connect", "" } do
				expect(protocol.parseRequest(value, decode).kind).to.equal("legacy")
			end
		end)

		it("should read every field of a JSON request", function()
			local parsed = protocol.parseRequest(
				'{"port": 34873, "confirm": "studio", "replace": true, "reconnect": false, "requestId": "abc"}',
				decode
			)

			expect(parsed.kind).to.equal("request")
			expect(parsed.request.port).to.equal(34873)
			expect(parsed.request.confirm).to.equal("studio")
			expect(parsed.request.replace).to.equal(true)
			expect(parsed.request.reconnect).to.equal(false)
			expect(parsed.request.requestId).to.equal("abc")
		end)

		it("should never let a request that doesn't say who wins overwrite anything", function()
			local parsed = protocol.parseRequest("{}", decode)

			expect(parsed.kind).to.equal("request")
			expect(parsed.request.confirm).to.equal("decide")
			expect(parsed.request.replace).to.equal(false)
			expect(parsed.request.reconnect).to.equal(true)
			expect(parsed.request.port).to.equal(nil)
			expect(parsed.request.requestId).to.equal(nil)
		end)

		it("should take true for the port in the Rojo panel", function()
			local parsed = protocol.parseRequest('{"port": true}', decode)
			expect(parsed.kind).to.equal("request")
			expect(parsed.request.port).to.equal(nil)
		end)

		it("should refuse requests that can't be carried out as written", function()
			local invalid = {
				'{"port": 0}',
				'{"port": 70000}',
				'{"port": 1.5}',
				'{"port": "34873"}',
				'{"confirm": "maybe"}',
				'{"confirm": true}',
				'{"replace": "yes"}',
				'{"reconnect": 1}',
				'{"requestId": true}',
				"{ not json",
			}

			for _, value in invalid do
				local parsed = protocol.parseRequest(value, decode)
				expect(parsed.kind).to.equal("invalid")
				expect(type(parsed.message)).to.equal("string")
			end
		end)

		it("should say which request it refused", function()
			local parsed = protocol.parseRequest('{"port": 0, "requestId": "abc"}', decode)
			expect(parsed.requestId).to.equal("abc")
		end)
	end)

	describe("parseDecision", function()
		it("should read the three decisions", function()
			for _, decision in { "files", "studio", "cancel" } do
				expect(protocol.parseDecision(decision, decode)).to.equal(decision)
			end
		end)

		it("should read a decision that says which prompt it answers", function()
			local decision, decisionId = protocol.parseDecision('{"decision": "studio", "decisionId": "abc"}', decode)
			expect(decision).to.equal("studio")
			expect(decisionId).to.equal("abc")
		end)

		it("should not read anything else as a decision", function()
			for _, value in { "Files", "yes", "", true, 1, '{"decision": "maybe"}', "{}" } do
				expect(protocol.parseDecision(value, decode)).to.equal(nil)
			end
		end)
	end)

	describe("classifyError", function()
		it("should keep the code of errors that have one", function()
			local code, message = protocol.classifyError(protocol.codedError("version_mismatch", "Wrong version"))
			expect(code).to.equal("version_mismatch")
			expect(message).to.equal("Wrong version")
		end)

		it("should read coded errors as their message", function()
			expect(tostring(protocol.codedError("place_not_served", "Wrong place"))).to.equal("Wrong place")
		end)

		it("should tell that the server is gone", function()
			local gone = {
				"Couldn't connect to the Rojo server.\nMake sure the server is running — use 'rojo serve' to run it!",
				"HTTP request timed out.",
				"WebSocket connection closed unexpectedly",
				"WebSocket error: 1006 - connection reset",
				"Server changed ID",
				-- Errors from HTTP requests are tables
				{ message = "Couldn't connect to the Rojo server." },
			}

			for _, err in gone do
				expect(protocol.classifyError(err)).to.equal("unreachable")
			end
		end)

		it("should tell that a prompt was aborted", function()
			expect(protocol.classifyError("Aborted Rojo sync operation")).to.equal("cancelled")
			expect(protocol.classifyError("AssetSync:800: Asset sync was aborted")).to.equal("cancelled")
		end)

		it("should report everything else as an error with its text", function()
			local code, message = protocol.classifyError("Cannot sync a model as a place.")
			expect(code).to.equal("error")
			expect(message).to.equal("Cannot sync a model as a place.")
		end)
	end)

	describe("reconnectDelay", function()
		it("should double from half a second up to five seconds", function()
			local delays = {}
			for attempt = 1, 7 do
				delays[attempt] = protocol.reconnectDelay(attempt)
			end

			expect(table.concat(delays, " ")).to.equal("0.5 1 2 4 5 5 5")
		end)
	end)

	describe("summarizePatch", function()
		local root = instance("Place1")
		local services = instance("ReplicatedStorage")
		local changedScript = instance("ReplicatedStorage.Shared.Util")
		local changedPart = instance("Workspace.Baseplate")
		local removedModel = instance("Workspace.OldModel", 4)

		local instanceMap = {
			fromIds = {
				ROOT = root,
				SERVICE = services,
				SCRIPT = changedScript,
				PART = changedPart,
			},
		}

		local function virtual(id: string, parent: string, name: string)
			return { Id = id, Parent = parent, Name = name, ClassName = "Folder", Properties = {}, Children = {} }
		end

		it("should count instances and list where they are", function()
			local summary = protocol.summarizePatch({
				added = {
					A = virtual("A", "SERVICE", "NewFolder"),
					B = virtual("B", "A", "Child"),
					C = virtual("C", "ROOT", "NewService"),
				},
				removed = { removedModel },
				updated = {
					{ id = "SCRIPT", changedProperties = { Source = { String = "return 1" } } },
					{ id = "PART", changedName = "Floor", changedProperties = {} },
				},
			}, instanceMap, root)

			expect(summary.scope).to.equal("project")

			-- Descendants count, but only the top of what's added or removed is listed
			expect(summary.counts.added).to.equal(3)
			expect(table.concat(summary.paths.added, " ")).to.equal("NewService ReplicatedStorage.NewFolder")

			expect(summary.counts.removed).to.equal(5)
			expect(table.concat(summary.paths.removed, " ")).to.equal("Workspace.OldModel")

			expect(summary.counts.changed).to.equal(2)
			expect(summary.counts.changedSource).to.equal(1)
			expect(table.concat(summary.paths.changed, " ")).to.equal("ReplicatedStorage.Shared.Util Workspace.Baseplate")
		end)

		it("should list at most 50 paths of each kind, but count everything", function()
			local updated = {}
			local fromIds = {}
			for index = 1, 120 do
				local id = "ID" .. index
				fromIds[id] = instance(string.format("Workspace.Part%03d", index))
				updated[index] = { id = id, changedProperties = { Anchored = { Bool = true } } }
			end

			local summary = protocol.summarizePatch(
				{ added = {}, removed = {}, updated = updated },
				{ fromIds = fromIds },
				root
			)

			expect(summary.counts.changed).to.equal(120)
			expect(#summary.paths.changed).to.equal(50)
			expect(summary.paths.changed[1]).to.equal("Workspace.Part001")
			expect(summary.paths.changed[50]).to.equal("Workspace.Part050")
		end)

		it("should leave out the name Studio gives the DataModel", function()
			local patch = {
				added = {},
				removed = {},
				updated = { { id = "ROOT", changedName = "my-project", changedProperties = {} } },
			}

			local summary = protocol.summarizePatch(patch, instanceMap, root)
			expect(summary.counts.changed).to.equal(0)
			expect(#summary.paths.changed).to.equal(0)
		end)
	end)

	describe("isTrivialPatch", function()
		local root = instance("Place1")
		local part = instance("Workspace.Baseplate")
		local instanceMap = { fromIds = { ROOT = root, PART = part } }

		local function patch(fields)
			return {
				added = fields.added or {},
				removed = fields.removed or {},
				updated = fields.updated or {},
			}
		end

		it("should not ask about nothing, or about the name of the DataModel", function()
			expect(protocol.isTrivialPatch(patch({}), instanceMap, root)).to.equal(true)
			expect(protocol.isTrivialPatch(
				patch({
					updated = { { id = "ROOT", changedName = "my-project", changedProperties = {} } },
				}),
				instanceMap,
				root
			)).to.equal(true)
		end)

		it("should ask about everything else", function()
			local differences = {
				patch({ added = { A = {} } }),
				patch({ removed = { part } }),
				patch({ updated = { { id = "PART", changedName = "Floor", changedProperties = {} } } }),
				patch({ updated = { { id = "ROOT", changedProperties = { Name = { String = "x" } } } } }),
				patch({ updated = { { id = "ROOT", changedClassName = "Folder", changedProperties = {} } } }),
			}

			for _, difference in differences do
				expect(protocol.isTrivialPatch(difference, instanceMap, root)).to.equal(false)
			end
		end)
	end)

	describe("summarizeAssets", function()
		it("should describe what asset sync asks about", function()
			local conflicts = {}
			for index = 1, 60 do
				conflicts[index] = { path = string.format("Models/Tree%02d.rbxm", index), reason = "bothChanged" }
			end

			local summary = protocol.summarizeAssets({
				path = "assets/ServerStorage",
				firstSync = false,
				toFs = 2,
				toStudio = 3,
				deleteFs = 1,
				deleteStudio = 4,
				upgrades = 5,
				conflicts = conflicts,
			})

			expect(summary.scope).to.equal("assets")
			expect(summary.container).to.equal("assets/ServerStorage")
			expect(summary.firstSync).to.equal(false)
			expect(summary.counts.conflicts).to.equal(60)
			expect(summary.counts.toFiles).to.equal(7)
			expect(summary.counts.deleteFiles).to.equal(1)
			expect(summary.counts.toStudio).to.equal(3)
			expect(summary.counts.deleteStudio).to.equal(4)
			expect(#summary.conflicts).to.equal(50)
			expect(summary.conflicts[1].path).to.equal("Models/Tree01.rbxm")
			expect(summary.conflicts[1].reason).to.equal("bothChanged")
		end)
	end)

	describe("encodeResult", function()
		it("should always have the same fields, with null for the ones without a value", function()
			local text = protocol.encodeResult({
				requestId = "abc",
				ok = false,
				code = "unreachable",
				message = 'Could not "connect"',
			}, encode)

			expect(text).to.equal(
				'{"requestId":"abc","ok":false,"address":null,"code":"unreachable",'
					.. '"message":"Could not \\"connect\\"","winner":null,"summary":null}'
			)
		end)

		it("should add what only some results have", function()
			local text = protocol.encodeResult({
				ok = true,
				code = "connected",
				reconnected = true,
			}, encode)

			local result = decode(text)
			expect(result.reconnected).to.equal(true)
			expect(result.decisions).to.equal(nil)
		end)
	end)

	describe("encodeState", function()
		it("should always have the same fields", function()
			local text = protocol.encodeState({
				state = "disconnected",
				since = 5,
				handshake = 2,
			}, encode)

			expect(text).to.equal(
				'{"state":"disconnected","port":null,"since":5,"lastError":null,"address":null,'
					.. '"projectName":null,"requestId":null,"confirm":null,"awaiting":null,"attempt":null,'
					.. '"studioSessionId":null,"handshake":2}'
			)
		end)
	end)
end
