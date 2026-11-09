return function()
	local ServerDiscovery = require(script.Parent.ServerDiscovery)

	describe("normalizePort", function()
		it("should accept ports as numbers or strings", function()
			expect(ServerDiscovery.normalizePort(34872)).to.equal("34872")
			expect(ServerDiscovery.normalizePort("34873")).to.equal("34873")
		end)

		it("should reject anything that isn't a port", function()
			for _, value in { "", "abc", "0", "70000", "1.5", -1, true } do
				expect(ServerDiscovery.normalizePort(value)).to.equal(nil)
			end
			expect(ServerDiscovery.normalizePort(nil)).to.equal(nil)
		end)
	end)

	describe("uniquePorts", function()
		it("should list every valid port once, in order", function()
			local ports = ServerDiscovery.uniquePorts({ 34873, "34871" }, { "34873", "", "abc", 34872 })
			expect(#ports).to.equal(3)
			expect(ports[1]).to.equal("34873")
			expect(ports[2]).to.equal("34871")
			expect(ports[3]).to.equal("34872")
		end)
	end)

	describe("same", function()
		local function server(port: string, projectName: string)
			return { host = "localhost", port = port, projectName = projectName }
		end

		it("should tell whether two lists show the same servers", function()
			local a = { server("34872", "game"), server("34873", "game (slot 1)") }
			local b = { server("34872", "game"), server("34873", "game (slot 1)") }
			expect(ServerDiscovery.same(a, b)).to.equal(true)
			expect(ServerDiscovery.same(a, { server("34872", "game") })).to.equal(false)
			expect(ServerDiscovery.same(a, { server("34872", "game"), server("34873", "other") })).to.equal(false)
			expect(ServerDiscovery.same(nil, nil)).to.equal(true)
			expect(ServerDiscovery.same(a, nil)).to.equal(false)
		end)
	end)
end
