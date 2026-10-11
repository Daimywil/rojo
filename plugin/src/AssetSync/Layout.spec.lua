return function()
	local Layout = require(script.Parent.Layout)

	local function emptyInstanceMap()
		return { fromInstances = {} }
	end

	local function make(className: string, name: string, parent: Instance?): Instance
		local instance = Instance.new(className)
		instance.Name = name
		instance.Parent = parent
		return instance
	end

	local function sortedKeys(dictionary)
		local keys = {}
		for key in dictionary do
			table.insert(keys, key)
		end
		table.sort(keys)
		return keys
	end

	local function invert(dictionary)
		local inverted = {}
		for key, value in dictionary do
			inverted[value] = key
		end
		return inverted
	end

	it("should turn plain folders into directories and everything else into files", function()
		local container = make("Folder", "Assets")
		local maps = make("Folder", "Maps - Bounty", container)
		local namek = make("Folder", "Namek", maps)
		local rock = make("Model", "Rock", namek)
		make("Part", "Part", rock)
		local tree = make("Model", "Tree", container)

		local layout = Layout.compute(container, emptyInstanceMap())

		expect(table.concat(sortedKeys(layout.directories), ",")).to.equal("Maps - Bounty,Maps - Bounty/Namek")
		expect(table.concat(sortedKeys(layout.units), ",")).to.equal("Maps - Bounty/Namek/Rock.rbxm,Tree.rbxm")
		expect(layout.units["Maps - Bounty/Namek/Rock.rbxm"]).to.equal(rock)
		expect(layout.units["Tree.rbxm"]).to.equal(tree)
		expect(layout.directories["Maps - Bounty/Namek"]).to.equal(namek)

		container:Destroy()
	end)

	it("should store folders with attributes, tags, or no children as files", function()
		local container = make("Folder", "Assets")

		local withAttribute = make("Folder", "Configured", container)
		withAttribute:SetAttribute("Speed", 10)
		make("Part", "Part", withAttribute)

		local withTag = make("Folder", "Tagged", container)
		withTag:AddTag("RojoAssetSyncTestTag")
		make("Part", "Part", withTag)

		make("Folder", "Empty", container)

		local layout = Layout.compute(container, emptyInstanceMap())

		expect(next(layout.directories)).to.equal(nil)
		expect(table.concat(sortedKeys(layout.units), ",")).to.equal("Configured.rbxm,Empty.rbxm,Tagged.rbxm")

		container:Destroy()
	end)

	it("should skip instances that Rojo manages and that aren't archivable", function()
		local container = make("Folder", "Assets")
		local managed = make("Folder", "TS", container)
		make("ModuleScript", "Module", managed)
		local unarchivable = make("Part", "Temporary", container)
		unarchivable.Archivable = false
		make("Part", "Kept", container)

		local instanceMap = emptyInstanceMap()
		instanceMap.fromInstances[managed] = "some-id"

		local layout = Layout.compute(container, instanceMap)

		expect(table.concat(sortedKeys(layout.units), ",")).to.equal("Kept.rbxm")
		expect(next(layout.directories)).to.equal(nil)

		container:Destroy()
	end)

	it("should give duplicate names unique paths in child order", function()
		local container = make("Folder", "Assets")
		local first = make("Part", "Part", container)
		local second = make("Part", "Part", container)

		local layout = Layout.compute(container, emptyInstanceMap())

		expect(layout.units["Part.rbxm"]).to.equal(first)
		expect(layout.units["Part~2.rbxm"]).to.equal(second)

		container:Destroy()
	end)

	it("should keep file names of reordered siblings with the same name", function()
		local container = make("Folder", "Assets")
		local first = make("Part", "Spawn", container)
		local second = make("Part", "Spawn", container)
		local instanceMap = emptyInstanceMap()

		local before = Layout.compute(container, instanceMap)
		expect(before.units["Spawn.rbxm"]).to.equal(first)
		expect(before.units["Spawn~2.rbxm"]).to.equal(second)

		local previousPaths = {}
		for path, instance in before.units do
			previousPaths[instance] = path
		end

		-- Moving the first one makes it the last child.
		first.Parent = nil
		first.Parent = container

		local after = Layout.compute(container, instanceMap, previousPaths, {})
		expect(after.units["Spawn.rbxm"]).to.equal(first)
		expect(after.units["Spawn~2.rbxm"]).to.equal(second)

		-- Without knowing the previous paths, child order decides.
		local fresh = Layout.compute(container, instanceMap)
		expect(fresh.units["Spawn.rbxm"]).to.equal(second)

		-- An instance that's renamed doesn't keep its old file name.
		second.Name = "Rock"
		local renamed = Layout.compute(container, instanceMap, previousPaths, {})
		expect(renamed.units["Rock.rbxm"]).to.equal(second)
		expect(renamed.units["Spawn.rbxm"]).to.equal(first)
		expect(renamed.units["Spawn~2.rbxm"]).to.equal(nil)

		container:Destroy()
	end)

	it("should keep file names of assets in renamed folders", function()
		local container = make("Folder", "Assets")
		local folder = make("Folder", "Trees", container)
		local first = make("Model", "Tree", folder)
		local second = make("Model", "Tree", folder)
		local third = make("Model", "Tree", folder)
		local instanceMap = emptyInstanceMap()

		local before = Layout.compute(container, instanceMap)
		expect(before.units["Trees/Tree~3.rbxm"]).to.equal(third)

		second.Parent = nil
		folder.Name = "Forest"

		local after = Layout.compute(container, instanceMap, invert(before.units), invert(before.directories))
		expect(after.directories["Forest"]).to.equal(folder)
		expect(after.units["Forest/Tree.rbxm"]).to.equal(first)
		expect(after.units["Forest/Tree~3.rbxm"]).to.equal(third)

		container:Destroy()
		second:Destroy()
	end)

	describe("incremental layouts", function()
		it("should only look at directories that are marked dirty", function()
			local container = make("Folder", "Assets")
			local a = make("Folder", "A", container)
			local rock = make("Part", "Rock", a)
			local b = make("Folder", "B", container)
			make("Part", "Tree", b)
			local instanceMap = emptyInstanceMap()

			local before = Layout.compute(container, instanceMap)
			local unitPaths, directoryPaths = invert(before.units), invert(before.directories)
			rock.Name = "Stone"

			local cached = Layout.compute(container, instanceMap, unitPaths, directoryPaths, nil, before.entries, {})
			expect(cached.units["A/Rock.rbxm"]).to.equal(rock)

			local updated =
				Layout.compute(container, instanceMap, unitPaths, directoryPaths, nil, before.entries, { [a] = true })
			expect(updated.units["A/Stone.rbxm"]).to.equal(rock)
			expect(updated.units["A/Rock.rbxm"]).to.equal(nil)
			expect(updated.units["B/Tree.rbxm"]).never.to.equal(nil)

			container:Destroy()
		end)

		it("should notice instances that moved out of a directory nobody marked", function()
			local container = make("Folder", "Assets")
			local a = make("Folder", "A", container)
			local rock = make("Part", "Rock", a)
			make("Part", "Kept", a)
			local b = make("Folder", "B", container)
			make("Part", "Tree", b)
			local instanceMap = emptyInstanceMap()

			local before = Layout.compute(container, instanceMap)
			rock.Parent = b

			local after = Layout.compute(
				container,
				instanceMap,
				invert(before.units),
				invert(before.directories),
				nil,
				before.entries,
				{ [b] = true }
			)
			expect(after.units["B/Rock.rbxm"]).to.equal(rock)
			expect(after.units["A/Rock.rbxm"]).to.equal(nil)
			expect(after.units["A/Kept.rbxm"]).never.to.equal(nil)

			container:Destroy()
		end)

		it("should tell whether a directory's entries are still right", function()
			local container = make("Folder", "Assets")
			local a = make("Folder", "A", container)
			local rock = make("Part", "Rock", a)
			local instanceMap = emptyInstanceMap()

			local layout = Layout.compute(container, instanceMap)
			local unitPaths, directoryPaths = invert(layout.units), invert(layout.directories)

			local current = Layout.entriesOf(container, a, "A", instanceMap, unitPaths, directoryPaths)
			expect(Layout.sameEntries(current, layout.entries[a])).to.equal(true)

			rock.Name = "Stone"
			current = Layout.entriesOf(container, a, "A", instanceMap, unitPaths, directoryPaths)
			expect(Layout.sameEntries(current, layout.entries[a])).to.equal(false)

			container:Destroy()
		end)
	end)

	describe("groups", function()
		it("should group the units of a directory in order, within limits", function()
			local container = make("Folder", "Assets")
			local parts = {}
			for index = 1, 70 do
				table.insert(parts, make("Part", "Part" .. index, container))
			end
			local layout = Layout.compute(container, emptyInstanceMap())

			local groups = Layout.groups(layout, function()
				return 1
			end)
			expect(#groups).to.equal(2)
			expect(#groups[1]).to.equal(64)
			expect(groups[1][1]).to.equal(parts[1])
			expect(groups[2][6]).to.equal(parts[70])

			-- Units with many instances are serialized on their own.
			groups = Layout.groups(layout, function(instance)
				return if instance == parts[2] then 5000 else 1
			end)
			expect(#groups[1]).to.equal(1)
			expect(groups[2][1]).to.equal(parts[2])
			expect(#groups[2]).to.equal(1)
			expect(groups[3][1]).to.equal(parts[3])

			container:Destroy()
		end)
	end)

	describe("hints", function()
		local function hash(character: string): string
			return string.rep(character, 32)
		end

		local function short(character: string): string
			return string.rep(character, 16)
		end

		it("should give assets the files they were synced to, whatever order they're in", function()
			local container = make("Folder", "Assets")
			local first = make("Model", "Tree", container)
			local third = make("Model", "Tree", container)
			local hashes = { [first] = hash("a"), [third] = hash("c") }

			-- The second tree was deleted, so the third one kept its name.
			local hints = Layout.hints({
				{ path = "Tree.rbxm", hashes = { short("a") } },
				{ path = "Tree~3.rbxm", hashes = { short("c") } },
			}, { "Tree.rbxm", "Tree~3.rbxm" }, hashes)

			local layout = Layout.compute(container, emptyInstanceMap(), nil, nil, hints)
			expect(layout.units["Tree.rbxm"]).to.equal(first)
			expect(layout.units["Tree~3.rbxm"]).to.equal(third)

			first.Parent = nil
			first.Parent = container

			layout = Layout.compute(container, emptyInstanceMap(), nil, nil, hints)
			expect(layout.units["Tree.rbxm"]).to.equal(first)
			expect(layout.units["Tree~3.rbxm"]).to.equal(third)

			container:Destroy()
		end)

		it("should give changed assets the names their family had", function()
			local container = make("Folder", "Assets")
			local first = make("Model", "Tree", container)
			local third = make("Model", "Tree", container)
			local new = make("Model", "Tree", container)
			local hashes = { [first] = hash("x"), [third] = hash("o"), [new] = hash("n") }

			local hints = Layout.hints({
				{ path = "Tree.rbxm", hashes = { short("a") } },
				{ path = "Tree~3.rbxm", hashes = { short("c"), short("o") } },
			}, { "Tree.rbxm", "Tree~3.rbxm", "Tree~4.rbxm" }, hashes)

			local layout = Layout.compute(container, emptyInstanceMap(), nil, nil, hints)

			-- The third tree went back to a version it was synced as before.
			expect(layout.units["Tree~3.rbxm"]).to.equal(third)
			-- The first tree changed, so it gets the name its family has left.
			expect(layout.units["Tree.rbxm"]).to.equal(first)
			-- New assets don't take the files of assets Studio doesn't have.
			expect(layout.units["Tree~2.rbxm"]).to.equal(new)

			container:Destroy()
		end)

		it("should give assets renamed to different capitalization their files back", function()
			local container = make("Folder", "Assets")
			local tree = make("Model", "Tree", container)

			local hints = Layout.hints({
				{ path = "tree.rbxm", hashes = { short("a") } },
			}, { "tree.rbxm" }, { [tree] = hash("b") })

			local layout = Layout.compute(container, emptyInstanceMap(), nil, nil, hints)
			expect(layout.units["tree.rbxm"]).to.equal(tree)

			-- Once the session is running, the file is renamed to match.
			local renamed = Layout.compute(container, emptyInstanceMap(), { [tree] = "tree.rbxm" }, {})
			expect(renamed.units["Tree.rbxm"]).to.equal(tree)

			container:Destroy()
		end)

		it("should give folders the directory their assets were synced in", function()
			local container = make("Folder", "Assets")
			local a = make("Folder", "Spawns", container)
			local aPart = make("Part", "Spawn", a)
			local b = make("Folder", "Spawns", container)
			local bPart = make("Part", "Spawn", b)
			local hashes = { [aPart] = hash("a"), [bPart] = hash("b") }

			local hints = Layout.hints({
				{ path = "Spawns/Spawn.rbxm", hashes = { short("b") } },
				{ path = "Spawns~2/Spawn.rbxm", hashes = { short("a") } },
			}, { "Spawns/Spawn.rbxm", "Spawns~2/Spawn.rbxm" }, hashes)

			local layout = Layout.compute(container, emptyInstanceMap(), nil, nil, hints)
			expect(layout.directories["Spawns"]).to.equal(b)
			expect(layout.directories["Spawns~2"]).to.equal(a)
			expect(layout.units["Spawns/Spawn.rbxm"]).to.equal(bPart)
			expect(layout.units["Spawns~2/Spawn.rbxm"]).to.equal(aPart)

			container:Destroy()
		end)

		it("should use the names of the files on disk the first time a place syncs", function()
			local container = make("Folder", "Assets")
			local first = make("Model", "Tree", container)
			local third = make("Model", "Tree", container)
			local hashes = { [first] = hash("a"), [third] = hash("c") }

			local hints = Layout.hints({}, { "Tree.rbxm", "Tree~3.rbxm" }, hashes)

			local layout = Layout.compute(container, emptyInstanceMap(), nil, nil, hints)
			expect(layout.units["Tree.rbxm"]).to.equal(first)
			expect(layout.units["Tree~3.rbxm"]).to.equal(third)

			container:Destroy()
		end)
	end)

	it("should find parent paths", function()
		expect(Layout.parentPath("a/b/c.rbxm")).to.equal("a/b")
		expect(Layout.parentPath("c.rbxm")).to.equal(nil)
	end)
end
