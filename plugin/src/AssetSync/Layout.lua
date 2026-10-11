--[[
	Decides which file every asset in a container is stored in.

	Plain folders become directories and every other instance becomes one
	`.rbxm` file holding it and all of its descendants. A folder is only plain
	if it has no attributes or tags and has children to sync; any other folder
	is stored as a file too, so that nothing about it is lost.
]]

local names = require(script.Parent.names)

local Layout = {}

local UNSYNCABLE_CLASSES = {
	-- Can't be serialized on their own.
	PackageLink = true,
	Terrain = true,
}

--[[
	Whether an instance is synced as an asset. Instances that Rojo syncs from
	the project are left to Rojo, instances that aren't archivable are never
	saved by Studio, and some instances can't be serialized on their own.
]]
function Layout.isSyncable(instance: Instance, instanceMap): boolean
	local success, syncable = pcall(function()
		return instance.Archivable and not UNSYNCABLE_CLASSES[instance.ClassName]
	end)

	return success
		and syncable
		-- The editor's camera moves all the time, so it would be written to
		-- disk constantly if assets were synced into Workspace.
		and instance ~= workspace.CurrentCamera
		and instanceMap.fromInstances[instance] == nil
end

--[[
	Whether an instance is stored as a directory rather than a single file.
]]
function Layout.isDirectory(instance: Instance, instanceMap): boolean
	if instance.ClassName ~= "Folder" then
		return false
	end

	if next(instance:GetAttributes()) ~= nil or #instance:GetTags() > 0 then
		return false
	end

	if not names.canBeDirectory(instance.Name) then
		return false
	end

	for _, child in instance:GetChildren() do
		if Layout.isSyncable(child, instanceMap) then
			return true
		end
	end

	return false
end

export type Entry = {
	instance: Instance,
	kind: "Directory" | "Unit",
	fileName: string,
}

export type Layout = {
	-- Every asset file and the instance stored in it.
	units: { [string]: Instance },
	-- Every directory and the folder it represents.
	directories: { [string]: Instance },
	-- The entries of every directory and of the container, in the order
	-- they're in.
	entries: { [Instance]: { Entry } },
}

--[[
	Returns the path of the directory an asset path is in, or nil if it's at the
	root of its container.
]]
function Layout.parentPath(path: string): string?
	return string.match(path, "^(.*)/[^/]*$")
end

local function fileNameOf(path: string): string
	return string.match(path, "[^/]+$") :: string
end

type Kind = "Directory" | "Unit"

-- What's known about the files in one directory.
type DirectoryHints = {
	-- Units the place synced, by the first 16 bytes of the hash of what they
	-- were last synced as...
	latest: { [string]: { string } },
	-- ...and of what they were synced as before that.
	older: { [string]: { string } },
	-- Names of the units and directories the place synced, by family, in
	-- dedup order.
	synced: { [string]: { string } },
	-- Names of the units and directories on disk, by family, in dedup order.
	files: { [string]: { string } },
	-- Every name in `synced` and `files`.
	names: { [string]: true },
}

export type Hints = {
	-- Whether the place never synced the container before.
	firstSync: boolean,
	-- What every unit serializes to now.
	hashes: { [Instance]: string },
	-- What's known about every directory, by path. The container is "".
	directories: { [string]: DirectoryHints },
}

--[[
	Collects what's known about the files of a container, so that Layout.compute
	can give assets the files they had before, even in a new session.

	- `synced` lists every asset the place synced before, with the first 16
	  bytes of the hashes of what it was synced as, latest first.
	- `files` lists every asset file on disk.
	- `hashes` has what every unit serializes to now.
	- `yield`, if given, is called regularly and may yield, to spread the
	  work over several frames.
]]
function Layout.hints(
	synced: { { path: string, hashes: { string } } },
	files: { string },
	hashes: { [Instance]: string },
	yield: (() -> ())?
): Hints
	local directories: { [string]: DirectoryHints } = {}
	-- Directory path -> pool -> name -> kind
	local known = {}

	local function directory(path: string): DirectoryHints
		local info = directories[path]
		if info == nil then
			info = { latest = {}, older = {}, synced = {}, files = {}, names = {} }
			directories[path] = info
			known[path] = { synced = {}, files = {} }
		end
		return info
	end

	-- Records the file at `path` and the directories it's in.
	local function register(path: string, pool: "synced" | "files")
		local current = path
		local kind: Kind = "Unit"

		while true do
			local parent = Layout.parentPath(current) or ""
			directory(parent)

			local recorded = known[parent][pool]
			local name = fileNameOf(current)
			if recorded[name] ~= nil then
				-- Its ancestors were recorded along with it.
				break
			end
			recorded[name] = kind

			if parent == "" then
				break
			end
			current = parent
			kind = "Directory"
		end
	end

	for _, asset in synced do
		if yield then
			yield()
		end
		register(asset.path, "synced")

		local info = directory(Layout.parentPath(asset.path) or "")
		local fileName = fileNameOf(asset.path)
		for index, hash in asset.hashes do
			local byHash = if index == 1 then info.latest else info.older
			local candidates = byHash[hash]
			if candidates == nil then
				candidates = {}
				byHash[hash] = candidates
			end
			table.insert(candidates, fileName)
		end
	end

	for _, path in files do
		if yield then
			yield()
		end
		register(path, "files")
	end

	for path, info in directories do
		for _, pool in { "synced", "files" } do
			local families = info[pool]
			local numbers = {}

			for name, kind in known[path][pool] do
				if yield then
					yield()
				end

				local family = names.fileFamilyKey(name, kind)
				if families[family] == nil then
					families[family] = {}
				end
				table.insert(families[family], name)
				info.names[name] = true

				local _, number = names.splitFileName(name, kind)
				numbers[name] = number
			end

			for _, members in families do
				table.sort(members, function(a, b)
					return numbers[a] < numbers[b]
				end)
			end
		end
	end

	return {
		firstSync = #synced == 0,
		hashes = hashes,
		directories = directories,
	}
end

--[[
	Counts how many of a folder's units were synced in a directory.
]]
local function countSyncedIn(folder: Instance, directory: DirectoryHints?, hints: Hints): number
	if directory == nil then
		return 0
	end

	local count = 0
	for _, child in folder:GetChildren() do
		local hash = hints.hashes[child]
		if hash ~= nil then
			local prefix = string.sub(hash, 1, 16)
			if directory.latest[prefix] ~= nil or directory.older[prefix] ~= nil then
				count += 1
			end
		end
	end

	return count
end

--[[
	Gives entries the names they had before this session, so that assets keep
	their files whatever order they're in now:

	1. Units with the contents a file was last synced with get that file, then
	   units with contents it was synced with before that.
	2. Folders get the directory most of their units were synced in.
	3. Anything else gets the names its family had, in order. If the place
	   never synced before, those are the names the files on disk have.

	Siblings with the same name are otherwise told apart by the order they're
	in, which changes when they're replaced or moved and doesn't survive
	deleting one of them.

	Known names that are left over stay free, so that new assets don't take
	the file of an asset Studio doesn't have.
]]
local function claimKnownNames(entries, namer, known: DirectoryHints, hints: Hints, directoryPath: string?)
	local pool = if hints.firstSync then known.files else known.synced

	-- Names are only compared ignoring case, so that an asset that was renamed
	-- to different capitalization while not syncing still gets its file back.
	local function claim(entry, fileName: string): boolean
		if names.fileFamilyKey(fileName, entry.kind) == entry.family and namer:reserve(fileName) then
			entry.fileName = fileName
			return true
		end
		return false
	end

	for _, entry in entries do
		entry.family = names.familyKey(entry.instance.Name, entry.kind)
	end

	for _, byHash in { known.latest, known.older } do
		for _, entry in entries do
			local hash = if entry.fileName == nil then hints.hashes[entry.instance] else nil
			local candidates = if hash ~= nil then byHash[string.sub(hash, 1, 16)] else nil

			for _, fileName in candidates or {} do
				if claim(entry, fileName) then
					break
				end
			end
		end
	end

	for _, entry in entries do
		if entry.fileName == nil and entry.kind == "Directory" then
			local best, bestCount = nil, 0
			for _, fileName in pool[entry.family] or {} do
				if namer:isFree(fileName) then
					local path = if directoryPath then directoryPath .. "/" .. fileName else fileName
					local count = countSyncedIn(entry.instance, hints.directories[path], hints)
					if count > bestCount then
						best, bestCount = fileName, count
					end
				end
			end

			if best ~= nil then
				claim(entry, best)
			end
		end
	end

	for _, entry in entries do
		if entry.fileName == nil then
			for _, fileName in pool[entry.family] or {} do
				if claim(entry, fileName) then
					break
				end
			end
		end
	end

	for fileName in known.names do
		namer:reserve(fileName)
	end
end

--[[
	Computes the entries of a single directory: which of its children are
	synced, whether they're directories, and what their file names are.
]]
local function computeEntries(
	container: Instance,
	parent: Instance,
	directoryPath: string?,
	instanceMap,
	previousUnitPaths: { [Instance]: string }?,
	previousDirectoryPaths: { [Instance]: string }?,
	hints: Hints?
): { Entry }
	local namer = names.SiblingNamer.new()
	local entries = {}

	for _, child in parent:GetChildren() do
		if Layout.isSyncable(child, instanceMap) then
			local kind = if Layout.isDirectory(child, instanceMap) then "Directory" else "Unit"
			table.insert(entries, { instance = child, kind = kind })
		end
	end

	-- Where this directory was before. A directory that didn't exist before
	-- has nothing in it that could keep its name.
	local previousDirectory = if parent ~= container and previousDirectoryPaths
		then previousDirectoryPaths[parent]
		else nil

	if parent == container or previousDirectory ~= nil then
		for _, entry in entries do
			local previousPaths = if entry.kind == "Directory" then previousDirectoryPaths else previousUnitPaths
			local previous = previousPaths and previousPaths[entry.instance]

			if previous ~= nil and Layout.parentPath(previous) == previousDirectory then
				local fileName = fileNameOf(previous)
				if names.isNameFor(fileName, entry.instance.Name, entry.kind) and namer:reserve(fileName) then
					entry.fileName = fileName
				end
			end
		end
	end

	local known = if hints then hints.directories[directoryPath or ""] else nil
	if known ~= nil then
		claimKnownNames(entries, namer, known, hints :: Hints, directoryPath)
	end

	for _, entry in entries do
		entry.fileName = entry.fileName or namer:assign(entry.instance.Name, entry.kind)
		entry.family = nil
	end

	return entries
end

local function isStillParentOf(parent: Instance, entries: { Entry }): boolean
	for _, entry in entries do
		if entry.instance.Parent ~= parent then
			return false
		end
	end
	return true
end

--[[
	Computes the layout of every asset in the given container.

	Siblings with the same name are told apart by `~N` suffixes, which are
	handed out in the order they're in. That order changes whenever one of them
	is moved or replaced, so instances keep the file names they had as long as
	those still fit their names:

	- Within a session, given the paths instances had before. These are kept
	  even if the directory they're in was renamed.
	- When a session starts, given hints about what was synced before (see
	  Layout.hints).

	Given the entries of the previous layout, only directories that are marked
	dirty are looked at again. Everything else is taken from those entries,
	which keeps updating the layout cheap however many assets there are.
]]
function Layout.compute(
	container: Instance,
	instanceMap,
	previousUnitPaths: { [Instance]: string }?,
	previousDirectoryPaths: { [Instance]: string }?,
	hints: Hints?,
	previousEntries: { [Instance]: { Entry } }?,
	dirty: { [Instance]: true }?
): Layout
	local units = {}
	local directories = {}
	local entriesByDirectory = {}

	local function visit(parent: Instance, directoryPath: string?)
		local entries = if previousEntries then previousEntries[parent] else nil

		-- Entries whose instances moved are recomputed even if nobody said so.
		if entries == nil or (dirty and dirty[parent]) or not isStillParentOf(parent, entries) then
			entries = computeEntries(
				container,
				parent,
				directoryPath,
				instanceMap,
				previousUnitPaths,
				previousDirectoryPaths,
				hints
			)
		end
		entriesByDirectory[parent] = entries

		for _, entry in entries do
			local path = if directoryPath then directoryPath .. "/" .. entry.fileName else entry.fileName

			if entry.kind == "Directory" then
				directories[path] = entry.instance
				visit(entry.instance, path)
			else
				units[path] = entry.instance
			end
		end
	end

	visit(container, nil)

	return {
		units = units,
		directories = directories,
		entries = entriesByDirectory,
	}
end

--[[
	Computes the entries of one directory from scratch, to check whether the
	entries a layout has for it are still right.
]]
function Layout.entriesOf(
	container: Instance,
	directory: Instance,
	directoryPath: string?,
	instanceMap,
	previousUnitPaths: { [Instance]: string }?,
	previousDirectoryPaths: { [Instance]: string }?
): { Entry }
	return computeEntries(
		container,
		directory,
		directoryPath,
		instanceMap,
		previousUnitPaths,
		previousDirectoryPaths,
		nil
	)
end

--[[
	Whether two lists of entries describe the same layout.
]]
function Layout.sameEntries(a: { Entry }, b: { Entry }): boolean
	if #a ~= #b then
		return false
	end

	for index, entry in a do
		local other = b[index]
		if entry.instance ~= other.instance or entry.kind ~= other.kind or entry.fileName ~= other.fileName then
			return false
		end
	end

	return true
end

-- Groups of units serialized together are kept small enough to serialize in
-- about a frame.
local MAX_GROUP_UNITS = 64
local MAX_GROUP_INSTANCES = 2000

--[[
	Splits the units of every directory into groups that are serialized
	together, in the order they're in. Serializing a whole group at once is
	much faster than serializing its units one by one, and its hash tells
	whether any of them changed.

	`sizeOf` returns how many instances a unit has. Groups only depend on what
	Studio has, so they come out the same every session until something in
	them changes.
]]
function Layout.groups(layout: Layout, sizeOf: (Instance) -> number): { { Instance } }
	local groups = {}

	for _, entries in layout.entries do
		local group, groupSize = {}, 0

		for _, entry in entries do
			if entry.kind == "Unit" then
				local size = sizeOf(entry.instance)
				if #group > 0 and (#group >= MAX_GROUP_UNITS or groupSize + size > MAX_GROUP_INSTANCES) then
					table.insert(groups, group)
					group, groupSize = {}, 0
				end

				table.insert(group, entry.instance)
				groupSize += size
			end
		end

		if #group > 0 then
			table.insert(groups, group)
		end
	end

	return groups
end

return Layout
