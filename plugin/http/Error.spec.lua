return function()
	local Error = require(script.Parent.Error)

	it("should keep percent signs in messages", function()
		local err = Error.new(Error.Kind.Unknown, "Rock%7E2.rbxm is 100% gone")

		expect(err.message).to.equal("Unknown HTTP error: Rock%7E2.rbxm is 100% gone")
	end)

	it("should keep percent signs in response bodies", function()
		local err = Error.fromResponse({
			code = 500,
			body = '<div style="width: 100%">Unicorn!</div>',
		})

		expect(err.type).to.equal(Error.Kind.Unknown)
		expect(err.message).to.equal('Unknown HTTP error: 500: <div style="width: 100%">Unicorn!</div>')
	end)
end
