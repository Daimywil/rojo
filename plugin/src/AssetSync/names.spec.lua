return function()
	local names = require(script.Parent.names)

	-- These cases are mirrored in src/assets/names.rs.
	local ENCODE_CASES = {
		{ "Tree", "Tree" },
		{ "Maps - Bounty", "Maps - Bounty" },
		{ "a:b", "a%3Ab" },
		{ "100%", "100%25" },
		{ "a~b", "a%7Eb" },
		{ "what?*", "what%3F%2A" },
		{ "<tag>", "%3Ctag%3E" },
		{ "back\\slash/fwd", "back%5Cslash%2Ffwd" },
		{ 'pipe|quote"', "pipe%7Cquote%22" },
		{ ".hidden", "%2Ehidden" },
		{ "trailing.", "trailing%2E" },
		{ " padded ", "%20padded%20" },
		{ "..", "%2E%2E" },
		{ ".", "%2E" },
		{ "tab\there", "tab%09here" },
		{ "CON", "%43ON" },
		{ "con", "%63on" },
		{ "nul.txt", "%6Eul.txt" },
		{ "COM1", "%43OM1" },
		{ "Lpt9", "%4Cpt9" },
		{ "CONSOLE", "CONSOLE" },
		{ "caf\u{e9}", "caf\u{e9}" },
		{ "\u{1F332}", "\u{1F332}" },
		{ "", "" },
	}

	describe("encodeName", function()
		it("should encode names the same way the server does", function()
			for _, case in ENCODE_CASES do
				expect(names.encodeName(case[1])).to.equal(case[2])
			end
		end)

		it("should escape names that aren't valid UTF-8 byte by byte", function()
			expect(names.encodeName("a\255b")).to.equal("a%FFb")
		end)
	end)

	describe("decodeName", function()
		it("should reverse encodeName", function()
			for _, case in ENCODE_CASES do
				expect(names.decodeName(case[2])).to.equal(case[1])
			end
		end)

		it("should reject names that aren't in canonical form", function()
			expect(names.decodeName("a%3ab")).to.equal(nil)
			expect(names.decodeName("%41")).to.equal(nil)
			expect(names.decodeName("bad%zz")).to.equal(nil)
			expect(names.decodeName("trunc%4")).to.equal(nil)
			expect(names.decodeName("a~2")).to.equal(nil)
			expect(names.decodeName("CON")).to.equal(nil)
		end)
	end)

	describe("folderNameFromDir", function()
		it("should strip dedup suffixes", function()
			expect(names.folderNameFromDir("Maps - Bounty")).to.equal("Maps - Bounty")
			expect(names.folderNameFromDir("Spawns~2")).to.equal("Spawns")
			expect(names.folderNameFromDir("a%3Ab~3")).to.equal("a:b")
		end)

		it("should reject invalid suffixes", function()
			expect(names.folderNameFromDir("v~1")).to.equal(nil)
			expect(names.folderNameFromDir("v~02")).to.equal(nil)
			expect(names.folderNameFromDir("")).to.equal(nil)
		end)
	end)

	describe("SiblingNamer", function()
		it("should give siblings unique names ignoring case", function()
			local namer = names.SiblingNamer.new()
			local entries = {
				{ "Part", "Unit", "Part.rbxm" },
				{ "Part", "Unit", "Part~2.rbxm" },
				{ "part", "Unit", "part~3.rbxm" },
				{ "Part", "Directory", "Part" },
				{ "Part.rbxm", "Directory", "Part.rbxm~2" },
				{ "", "Unit", "_.rbxm" },
				{ "_", "Unit", "_~2.rbxm" },
				{ "\u{c4}pfel", "Unit", "\u{c4}pfel.rbxm" },
				{ "\u{e4}pfel", "Unit", "\u{e4}pfel~2.rbxm" },
				{ "\u{d6}l", "Unit", "\u{d6}l.rbxm" },
			}

			for _, entry in entries do
				expect(namer:assign(entry[1], entry[2])).to.equal(entry[3])
			end
		end)

		it("should truncate long unit names without cutting escape sequences", function()
			local namer = names.SiblingNamer.new()
			local assigned = namer:assign(string.rep("%", 100), "Unit")
			local stem = string.sub(assigned, 1, -6)

			expect(string.sub(assigned, -5)).to.equal(".rbxm")
			expect(#stem <= 200).to.equal(true)
			expect(#stem % 3).to.equal(0)
		end)
	end)

	describe("SiblingNamer:reserve", function()
		it("should keep reserved names away from other siblings", function()
			local namer = names.SiblingNamer.new()

			expect(namer:reserve("Part~3.rbxm")).to.equal(true)
			expect(namer:reserve("part~3.rbxm")).to.equal(false)

			expect(namer:assign("Part", "Unit")).to.equal("Part.rbxm")
			expect(namer:assign("Part", "Unit")).to.equal("Part~2.rbxm")
			expect(namer:assign("Part", "Unit")).to.equal("Part~4.rbxm")
		end)
	end)

	describe("splitFileName", function()
		it("should split off dedup suffixes and extensions", function()
			local stem, number = names.splitFileName("Part~2.rbxm", "Unit")
			expect(stem).to.equal("Part")
			expect(number).to.equal(2)

			stem, number = names.splitFileName("Part.rbxm", "Unit")
			expect(stem).to.equal("Part")
			expect(number).to.equal(1)

			stem, number = names.splitFileName("Dir~3", "Directory")
			expect(stem).to.equal("Dir")
			expect(number).to.equal(3)

			stem, number = names.splitFileName("v~1.rbxm", "Unit")
			expect(stem).to.equal("v~1")
			expect(number).to.equal(1)
		end)
	end)

	describe("isNameFor", function()
		it("should accept any dedup suffix of the right name", function()
			expect(names.isNameFor("Part.rbxm", "Part", "Unit")).to.equal(true)
			expect(names.isNameFor("Part~5.rbxm", "Part", "Unit")).to.equal(true)
			expect(names.isNameFor("a%3Ab~2", "a:b", "Directory")).to.equal(true)
		end)

		it("should reject names of other instances or kinds", function()
			expect(names.isNameFor("Part~5.rbxm", "Rock", "Unit")).to.equal(false)
			expect(names.isNameFor("Part", "Part", "Unit")).to.equal(false)
			expect(names.isNameFor("Part.rbxm", "Part", "Directory")).to.equal(false)
		end)
	end)

	describe("fileFamilyKey", function()
		it("should match the familyKey of the names a file name fits", function()
			expect(names.fileFamilyKey("Part~3.rbxm", "Unit")).to.equal(names.familyKey("Part", "Unit"))
			expect(names.fileFamilyKey("part.rbxm", "Unit")).to.equal(names.familyKey("Part", "Unit"))
			expect(names.fileFamilyKey("a%3Ab~2", "Directory")).to.equal(names.familyKey("a:b", "Directory"))
			expect(names.fileFamilyKey("Part", "Directory")).never.to.equal(names.familyKey("Part", "Unit"))
		end)
	end)

	describe("comparePaths", function()
		it("should sort paths the same way the server does", function()
			-- Mirrored in src/assets/names.rs.
			local paths = {
				"Tree~10.rbxm",
				"Tree~2.rbxm",
				"tree~3.rbxm",
				"Tree.rbxm",
				"Spawns~10/A.rbxm",
				"Spawns~2/A.rbxm",
				"Spawns/B.rbxm",
				"Spawns/A.rbxm",
				"Apple.rbxm",
			}
			table.sort(paths, names.comparePaths)

			expect(table.concat(paths, ",")).to.equal(table.concat({
				"Apple.rbxm",
				"Spawns/A.rbxm",
				"Spawns/B.rbxm",
				"Spawns~2/A.rbxm",
				"Spawns~10/A.rbxm",
				"Tree.rbxm",
				"Tree~2.rbxm",
				"tree~3.rbxm",
				"Tree~10.rbxm",
			}, ","))
		end)
	end)

	describe("pathSortKey", function()
		it("should sort paths the same way comparePaths does", function()
			local paths = {
				"Tree~10.rbxm",
				"Tree~2.rbxm",
				"tree~3.rbxm",
				"Tree.rbxm",
				"Tree/Leaf.rbxm",
				"Tree~2/Leaf.rbxm",
				"Trees.rbxm",
				"Spawns~10/A.rbxm",
				"Spawns~2/A.rbxm",
				"Spawns/B.rbxm",
				"Spawns/A.rbxm",
				"Spawns/Sub/A.rbxm",
				"Apple.rbxm",
				"a%3Ab.rbxm",
				"\u{c4}pfel.rbxm",
			}

			local byComparison = table.clone(paths)
			table.sort(byComparison, names.comparePaths)

			local byKey = table.clone(paths)
			table.sort(byKey, function(a, b)
				return names.pathSortKey(a) < names.pathSortKey(b)
			end)

			expect(table.concat(byKey, ",")).to.equal(table.concat(byComparison, ","))
		end)
	end)

	describe("canBeDirectory", function()
		it("should only allow names that can be decoded from a directory name", function()
			expect(names.canBeDirectory("Maps")).to.equal(true)
			expect(names.canBeDirectory("")).to.equal(false)
			expect(names.canBeDirectory(string.rep("%", 100))).to.equal(false)
			expect(names.canBeDirectory("a\255b")).to.equal(false)
		end)
	end)
end
