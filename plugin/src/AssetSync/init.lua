--[[
	Two-way sync of assets between Studio and the filesystem.

	The server mirrors the children of every instance declared with `$assets`
	to a directory, where plain folders become directories and every other
	instance becomes one .rbxm file (see Layout). Assets move between Studio
	and the server as whole instances serialized by SerializationService, which
	is fast and preserves every property, unlike syncing them property by
	property the way Rojo syncs scripts.

	- When starting, every asset is hashed and the server works out what changed
	  on either side since they last agreed (see src/assets/plan.rs). Only
	  assets changed on both sides need the user to decide.
	- Afterwards, changes made in Studio are written to files, and changes made
	  to files, for example by git, are applied in Studio.

	All work happens on one worker thread, one task at a time, so applying file
	changes and writing Studio changes never step on each other.
]]

local ChangeHistoryService = game:GetService("ChangeHistoryService")
local CollectionService = game:GetService("CollectionService")
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local Selection = game:GetService("Selection")
local ServerStorage = game:GetService("ServerStorage")

local Packages = script.Parent.Parent.Packages
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Layout = require(script.Layout)
local names = require(script.names)
local serializer = require(script.serializeMany)

-- How long an asset has to go without changing before it's written. Dragging
-- a part changes it every frame, which doesn't need to be written every frame.
local QUIET_PERIOD = 0.3
-- Assets that never stop changing are still written this often.
local MAX_DIRTY_AGE = 2
-- How long it takes to re-check every asset for changes that no event told us
-- about, like attributes set by another plugin...
local SWEEP_PERIOD = 120
-- ...and every directory for changes to which file each asset is stored in.
local LAYOUT_SWEEP_PERIOD = 60
-- File changes are applied once none arrived for this long...
local FILE_CHANGE_QUIET_PERIOD = 0.1
-- ...or once the first one has been waiting for this long. Applying changes
-- has a cost of its own, so a whole git checkout is best applied at once.
local MAX_FILE_CHANGE_DELAY = 3
local UPLOAD_BATCH_BYTES = 8 * 1024 * 1024
local READ_BATCH_SIZE = 200
local RETRY_DELAY = 3
-- How long work spread over frames may run before Studio gets to draw one.
local FRAME_WORK_BUDGET = 0.008
-- Unpublished places all have the place ID 0, so they're told apart by a key
-- stored in the place.
local LOCAL_PLACE_ATTRIBUTE = "__RojoAssetSyncPlace"

--[[
	Reads an attribute that asset sync stores in the place. They're kept in
	ServerStorage, which unlike Workspace is never sent to players, but older
	versions kept them in Workspace, so that's checked too.
]]
local function getPlaceAttribute(name: string): any
	local value = ServerStorage:GetAttribute(name)
	if value == nil then
		value = workspace:GetAttribute(name)
	end
	return value
end

local function setPlaceAttribute(name: string, value: any)
	ServerStorage:SetAttribute(name, value)
	if workspace:GetAttribute(name) ~= nil then
		workspace:SetAttribute(name, nil)
	end
end

local function errorToString(err): string
	if typeof(err) == "table" and err.message ~= nil then
		return tostring(err.message)
	end
	return tostring(err)
end

--[[
	Returns a function that yields once the work done since Studio last drew a
	frame took longer than `budget` seconds, so long loops don't make a single
	frame long. Only for work that doesn't change the game, since the game can
	change while it yields.
]]
local function frameBudget(budget: number): () -> ()
	local started = os.clock()
	return function()
		if os.clock() - started > budget then
			task.wait()
			started = os.clock()
		end
	end
end

local function isEncodingServiceAvailable(): boolean
	return pcall(function()
		local EncodingService = game:GetService("EncodingService")
		return EncodingService.ComputeBufferHash
	end)
end

local AssetSync = {}
AssetSync.__index = AssetSync

export type Conflict = {
	path: string,
	-- Why it's unclear which side is right: "bothChanged", "missingInStudio",
	-- "deletedOnDisk", "unsavedPlace", or "newInUnsavedPlace".
	reason: string,
}

export type Summary = {
	path: string,
	firstSync: boolean,
	toFs: number,
	toStudio: number,
	deleteFs: number,
	deleteStudio: number,
	conflicts: { Conflict },
}

--[[
	Options:
	- apiContext: the ApiContext of the session.
	- instanceMap: the session's InstanceMap, used to find containers and to
	  leave instances that Rojo syncs from the project alone.
	- confirm: called with a Summary when the user needs to decide what to do
	  on start. Yields until they do, and returns "KeepStudio", "KeepFiles",
	  or "Abort".
	- notify: called with short messages worth showing to the user.
]]
function AssetSync.new(options)
	local self = {
		__apiContext = options.apiContext,
		__instanceMap = options.instanceMap,
		__confirm = options.confirm,
		__notify = options.notify or function() end,

		-- Identifies our requests, so we can ignore changes we caused.
		__clientId = HttpService:GenerateGUID(false),
		__placeId = tostring(game.PlaceId),

		__containers = {},
		__containerById = {},
		__containerByInstance = {},

		__connections = {},
		__selectionConnections = {},
		__fsChanges = {},
		__firstFsChangeAt = 0,
		__lastFsChangeAt = 0,
		__running = false,
		__sweepQueue = {},
		__sweepBudget = 0,
		__layoutSweepQueue = {},
		__layoutSweepBudget = 0,
		__retryAt = 0,

		-- What __locate found for instances this frame, by instance. See
		-- __locateCached.
		__located = {},
	}

	return setmetatable(self, AssetSync)
end

--[[
	Syncs every container and then keeps them in sync until stopped. The
	returned promise resolves once the initial sync is done.
]]
function AssetSync:start()
	return Promise.new(function(resolve, reject)
		local success, err = pcall(function()
			self:__start()
		end)

		if success then
			resolve()
		else
			self:stop()
			reject(errorToString(err))
		end
	end)
end

function AssetSync:stop()
	self.__running = false

	for _, connection in self.__connections do
		connection:Disconnect()
	end
	table.clear(self.__connections)

	for _, connection in self.__selectionConnections do
		connection:Disconnect()
	end
	table.clear(self.__selectionConnections)

	for _, container in self.__containers do
		for _, connections in container.unitConnections do
			for _, connection in connections do
				connection:Disconnect()
			end
		end
		table.clear(container.unitConnections)

		for _, connection in container.directoryConnections do
			connection:Disconnect()
		end
		table.clear(container.directoryConnections)
	end

	self.__apiContext:disconnectAssetsWebSocket()
end

function AssetSync:isRunning(): boolean
	return self.__running
end

function AssetSync:__start()
	local info = self.__apiContext:assets():expect()
	if #info.containers == 0 then
		return
	end

	if RunService:IsRunning() then
		-- Changes made while playtesting must never end up in files.
		Log.info("Not syncing assets because the game is running")
		return
	end

	if not isEncodingServiceAvailable() then
		error("Syncing assets needs a newer version of Roblox Studio that has EncodingService:ComputeBufferHash.")
	end

	if game.PlaceId == 0 then
		local key = getPlaceAttribute(LOCAL_PLACE_ATTRIBUTE)
		if typeof(key) ~= "string" then
			key = HttpService:GenerateGUID(false)
		end
		if ServerStorage:GetAttribute(LOCAL_PLACE_ATTRIBUTE) ~= key then
			setPlaceAttribute(LOCAL_PLACE_ATTRIBUTE, key)
		end
		self.__placeId = "local-" .. key
	end

	-- The token of the session that synced this place last, if the place was
	-- saved after it. See src/assets/state.rs.
	self.__sessionAttribute = info.sessionAttribute
	local storedSession = getPlaceAttribute(info.sessionAttribute)
	self.__placeSession = if typeof(storedSession) == "string" then storedSession else nil

	for _, containerInfo in info.containers do
		local instance = self.__instanceMap.fromIds[containerInfo.id]
		if instance == nil then
			error(
				string.format(
					"Could not find the instance that the assets in %s are synced into. "
						.. "Make sure the project's tree describes it.",
					containerInfo.path
				)
			)
		end

		local container = {
			id = containerInfo.id,
			path = containerInfo.path,
			instance = instance,

			-- Every asset file we know of, and what we last synced to it.
			-- `hash` is the hash of what Studio serialized the asset to.
			units = {},
			unitPaths = {},
			directories = {},
			directoryPaths = {},

			-- Assets changed in Studio that haven't been written yet. Maps
			-- each one to when it first and last changed.
			dirtyUnits = {},
			-- When the layout last changed without being recomputed.
			structureDirty = nil,
			-- The entries of every directory as of the last layout, and the
			-- directories whose entries may have changed since. Only those
			-- are looked at when the layout is recomputed.
			layoutEntries = {},
			dirtyDirectories = {},
			-- Files that should be deleted, but deleting them failed.
			pendingRemovals = {},
			-- When to try writing again after writing failed.
			retryAt = nil,

			unitConnections = {},
			directoryConnections = {},
		}

		table.insert(self.__containers, container)
		self.__containerById[container.id] = container
		self.__containerByInstance[instance] = container
	end

	for _, container in self.__containers do
		self:__initialSync(container)
	end

	-- Studio now has everything this session knows about. If the place isn't
	-- saved after this, it won't have this token next time, which tells the
	-- server that Studio lost whatever was synced in this session.
	self.__apiContext
		:assetsSession({
			placeId = self.__placeId,
			session = self.__clientId,
		})
		:expect()
	setPlaceAttribute(self.__sessionAttribute, self.__clientId)

	self.__running = true
	self:__connectListeners()

	self.__apiContext
		:connectAssetsWebSocket(info.messageCursor, function(packet)
			self:__onSocketPacket(packet)
		end)
		:catch(function(err)
			if self.__running then
				Log.warn("Lost the connection for asset changes: {}", errorToString(err))
				self.__notify("Asset sync stopped: lost the connection to the Rojo server.")
				self:stop()
			end
		end)

	task.spawn(function()
		self:__workerLoop()
	end)
end

--[[
	Serializes the assets with the given paths whose hashes are known but that
	weren't serialized on their own yet, so that they can be uploaded.
]]
local function serializeMissing(
	paths: { string },
	serialized: { [string]: serializer.Result },
	units: { [string]: Instance }
)
	local missing, instances = {}, {}
	for _, path in paths do
		local result = serialized[path]
		if result ~= nil and result.data == nil and units[path] ~= nil then
			table.insert(missing, path)
			table.insert(instances, units[path])
		end
	end

	local results = serializer.serializeMany(instances, true)
	for index, path in missing do
		if results[index].hash ~= nil then
			serialized[path] = results[index]
		end
	end
end

--[[
	Uploads the serialized assets with the given paths so the server can
	compare or write them.
]]
function AssetSync:__upload(container, paths: { string }, serialized: { [string]: serializer.Result })
	local batch = {}
	local batchBytes = 0

	local function flush()
		if #batch == 0 then
			return
		end

		self.__apiContext
			:assetsStage({
				clientId = self.__clientId,
				container = container.id,
				assets = batch,
			})
			:expect()

		batch = {}
		batchBytes = 0
	end

	for _, path in paths do
		local result = serialized[path]
		if result == nil or result.data == nil then
			continue
		end

		table.insert(batch, { path = path, data = result.data })
		batchBytes += buffer.len(result.data)

		if batchBytes >= UPLOAD_BATCH_BYTES then
			flush()
		end
	end

	flush()
end

--[[
	Serializes the given instances, adding the results to `results`.
]]
local function serializeInto(results: { [Instance]: serializer.Result }, instances: { Instance })
	local serialized = serializer.serializeMany(instances, true)
	for index, instance in instances do
		results[instance] = serialized[index]
	end
end

type Group = {
	units: { Instance },
	-- The hash of the group's units serialized together, if that worked.
	hash: string?,
}

--[[
	Computes the layout of a container at the start of a session, so that every
	asset gets the file it had before, and finds out what every asset in it
	serializes to. Returns the layout, what every unit serializes to, and the
	groups the units were serialized in.

	Serializing every asset on its own is most of the time this takes, so
	assets are serialized in groups first. A group whose hash is the same as
	when it was last synced hasn't changed, so the hashes its units had then
	are still right. Only the units of groups that changed are serialized on
	their own.
]]
function AssetSync:__initialLayout(container): (Layout.Layout, { [Instance]: serializer.Result }, { Group })
	-- Which instances are units doesn't depend on their file names, so they
	-- can be serialized before the names are decided.
	local unitLayout = Layout.compute(container.instance, self.__instanceMap)

	local known = self.__apiContext
		:assetsKnown({
			placeId = self.__placeId,
			container = container.id,
		})
		:expect()

	local recordedGroups = {}
	for _, group in known.groups do
		local unitHashes = table.create(#group.units)
		for index, hash in group.units do
			unitHashes[index] = buffer.tostring(hash)
		end
		recordedGroups[buffer.tostring(group.hash)] = unitHashes
	end

	local check = frameBudget(FRAME_WORK_BUDGET)
	local groups: { Group } = {}
	local groupedUnits = Layout.groups(unitLayout, function(instance)
		check()
		return #instance:GetDescendants() + 1
	end)
	for index, units in groupedUnits do
		groups[index] = { units = units }
	end

	-- The hashes of groups are needed to remember them for next time even
	-- if there's nothing to compare them with yet.
	local groupResults = serializer.serializeGroups(groupedUnits)
	local results = {}
	local ungrouped = {}

	for index, group in groups do
		local hash = groupResults[index].hash
		group.hash = hash

		local recorded = if hash ~= nil then recordedGroups[hash] else nil
		if recorded ~= nil and #recorded == #group.units then
			for unitIndex, instance in group.units do
				results[instance] = { hash = recorded[unitIndex] }
			end
		elseif hash ~= nil and #group.units == 1 then
			-- A group of one is serialized exactly like its unit on its own.
			results[group.units[1]] = { hash = hash }
		else
			for _, instance in group.units do
				table.insert(ungrouped, instance)
			end
		end
	end

	Log.debug(
		"Serialized the assets in {} in {} groups, and {} assets in groups that changed on their own",
		container.path,
		#groups,
		#ungrouped
	)
	serializeInto(results, ungrouped)

	local hashes = {}
	for instance, result in results do
		hashes[instance] = result.hash
	end

	local synced = table.create(#known.synced)
	for index, asset in known.synced do
		local assetHashes = table.create(#asset.hashes)
		for hashIndex, hash in asset.hashes do
			assetHashes[hashIndex] = buffer.tostring(hash)
		end
		synced[index] = { path = asset.path, hashes = assetHashes }
	end

	task.wait()
	local hints = Layout.hints(synced, known.files, hashes, frameBudget(FRAME_WORK_BUDGET))
	task.wait()
	local layout = Layout.compute(container.instance, self.__instanceMap, nil, nil, hints)

	-- Serializing yields, so instances may have been added in the meantime.
	local added = {}
	for _, instance in layout.units do
		if results[instance] == nil then
			table.insert(added, instance)
		end
	end
	if #added > 0 then
		serializeInto(results, added)
	end

	return layout, results, groups
end

--[[
	Tells the server the hashes of groups of units that are in sync now, so the
	next session can tell they didn't change without serializing them on their
	own. Groups with units that the sync replaced or deleted aren't in sync
	anymore and are left out.
]]
function AssetSync:__recordGroups(
	container,
	groups: { Group },
	results: { [Instance]: serializer.Result },
	changed: { [Instance]: true }
)
	local records = {}

	for _, group in groups do
		-- Groups of one don't need to be remembered, since their hash is the
		-- hash of their unit.
		if group.hash == nil or #group.units < 2 then
			continue
		end

		local unitHashes = table.create(#group.units)
		for index, instance in group.units do
			local result = results[instance]
			if changed[instance] or result == nil or result.hash == nil then
				unitHashes = nil
				break
			end
			unitHashes[index] = buffer.fromstring(result.hash)
		end

		if unitHashes ~= nil then
			table.insert(records, { hash = buffer.fromstring(group.hash), units = unitHashes })
		end
	end

	self.__apiContext
		:assetsGroups({
			placeId = self.__placeId,
			container = container.id,
			groups = records,
		})
		:expect()
end

function AssetSync:__initialSync(container)
	local started = os.clock()
	local layout, results, groups = self:__initialLayout(container)

	local serialized = {}
	local skipped = {}
	local count = 0

	for path, instance in layout.units do
		count += 1
		local result = results[instance]
		if result.hash ~= nil then
			serialized[path] = result
		else
			table.insert(skipped, path)
			Log.warn("Could not serialize asset {}: {}", instance:GetFullName(), result.err)
		end
	end

	local function hashes()
		local list = {}
		for path, result in serialized do
			table.insert(list, { path = path, hash = buffer.fromstring(result.hash) })
		end
		return list
	end

	local planRequest = {
		clientId = self.__clientId,
		placeId = self.__placeId,
		session = self.__placeSession,
		container = container.id,
		assets = hashes(),
		skipped = skipped,
	}

	local plan = self.__apiContext:assetsPlan(planRequest):expect()
	if #plan.needUpload > 0 then
		Log.debug("Uploading {} assets to compare them", #plan.needUpload)

		-- Serializing these again may find that they changed in the meantime,
		-- so the plan is made with their latest hashes.
		serializeMissing(plan.needUpload, serialized, layout.units)
		planRequest.assets = hashes()

		self:__upload(container, plan.needUpload, serialized)
		plan = self.__apiContext:assetsPlan(planRequest):expect()

		if #plan.needUpload > 0 then
			-- Everything that was asked for was uploaded, so this should never
			-- happen. These assets are left alone until they change again.
			Log.warn("The Rojo server still needs {} assets after uploading them", #plan.needUpload)
		end
	end

	local toFs, deleteFs, toStudio, deleteStudio, conflicts = {}, {}, {}, {}, {}
	local restored = 0
	for _, entry in plan.actions do
		if entry.action == "toFs" then
			table.insert(toFs, entry.path)
		elseif entry.action == "deleteFs" then
			table.insert(deleteFs, entry.path)
		elseif entry.action == "toStudio" then
			table.insert(toStudio, entry.path)
			if entry.reason == "outdatedInStudio" then
				restored += 1
			end
		elseif entry.action == "deleteStudio" then
			table.insert(deleteStudio, entry.path)
		elseif entry.action == "conflict" then
			table.insert(conflicts, { path = entry.path, reason = entry.reason or "bothChanged" })
		end
	end

	-- Merging two sides that never synced before can mix up two unrelated
	-- sets of assets, for example after connecting to the wrong place, so the
	-- user gets to confirm that too.
	local mergesUnrelated = plan.firstSync and (#toFs > 0 or #deleteFs > 0) and (#toStudio > 0 or #deleteStudio > 0)

	if #conflicts > 0 or mergesUnrelated then
		table.sort(conflicts, function(a, b)
			return a.path < b.path
		end)

		local decision = self.__confirm({
			path = container.path,
			firstSync = plan.firstSync,
			toFs = #toFs,
			toStudio = #toStudio,
			deleteFs = #deleteFs,
			deleteStudio = #deleteStudio,
			conflicts = conflicts,
		})

		if decision == "KeepStudio" then
			for _, conflict in conflicts do
				if serialized[conflict.path] ~= nil then
					table.insert(toFs, conflict.path)
				else
					table.insert(deleteFs, conflict.path)
				end
			end
		elseif decision == "KeepFiles" then
			-- Files that don't exist are deleted from Studio when they're read.
			for _, conflict in conflicts do
				table.insert(toStudio, conflict.path)
			end
		else
			error("Asset sync was aborted")
		end
	end

	if #toFs > 0 or #deleteFs > 0 then
		self.__apiContext
			:assetsApply({
				clientId = self.__clientId,
				placeId = self.__placeId,
				container = container.id,
				toFs = toFs,
				deleteFs = deleteFs,
			})
			:expect()
	end

	-- What's in Studio now is what we know of, apart from what the files
	-- change below.
	for path, instance in layout.units do
		local result = serialized[path]
		self:__setUnit(container, path, instance, if result then result.hash else nil)
	end
	for path, instance in layout.directories do
		self:__setDirectory(container, path, instance)
	end
	container.layoutEntries = layout.entries
	container.dirtyDirectories = {}

	local replaced = {}
	for _, paths in { toStudio, deleteStudio } do
		for _, path in paths do
			local instance = layout.units[path]
			if instance ~= nil then
				replaced[instance] = true
			end
		end
	end

	if #toStudio > 0 or #deleteStudio > 0 then
		self:__applyFileChanges(container, toStudio, deleteStudio)
	end

	self:__recordGroups(container, groups, results, replaced)

	Log.info(
		"Synced {} assets in {} in {}s ({} written to files, {} deleted from files, {} applied to Studio, {} deleted from Studio)",
		count,
		container.path,
		string.format("%.1f", os.clock() - started),
		#toFs,
		#deleteFs,
		#toStudio,
		#deleteStudio
	)

	if restored > 0 then
		local message = string.format(
			"Restored %d assets in %s from files, because the place wasn't saved after they were last synced.",
			restored,
			container.path
		)
		Log.info(message)
		self.__notify(message)
	end

	local changed = #toFs + #deleteFs + #toStudio + #deleteStudio
	if changed > 0 then
		self.__notify(
			string.format(
				"Synced assets in %s: %d changed on disk, %d changed in Studio.",
				container.path,
				#toFs + #deleteFs,
				#toStudio + #deleteStudio
			)
		)
	end
end

--[[
	Records that the asset at `path` is `instance`, as last synced with `hash`.
]]
function AssetSync:__setUnit(container, path: string, instance: Instance, hash: string?)
	local previous = container.units[path]
	if previous ~= nil and previous.instance ~= instance then
		self:__forgetUnit(container, path)
	end

	-- The path this instance had before may belong to another instance by
	-- now, when assets swapped paths.
	local existingPath = container.unitPaths[instance]
	if existingPath ~= nil and existingPath ~= path then
		local existing = container.units[existingPath]
		if existing ~= nil and existing.instance == instance then
			container.units[existingPath] = nil
		end
	end

	container.units[path] = { instance = instance, hash = hash }
	container.unitPaths[instance] = path
	table.clear(self.__located)

	if container.unitConnections[instance] == nil and self.__running then
		self:__connectUnit(container, instance)
	end
end

function AssetSync:__forgetUnit(container, path: string)
	local entry = container.units[path]
	if entry == nil then
		return
	end

	container.units[path] = nil
	table.clear(self.__located)
	if container.unitPaths[entry.instance] == path then
		container.unitPaths[entry.instance] = nil

		local connections = container.unitConnections[entry.instance]
		if connections ~= nil then
			for _, connection in connections do
				connection:Disconnect()
			end
			container.unitConnections[entry.instance] = nil
		end
	end
end

function AssetSync:__setDirectory(container, path: string, instance: Instance)
	local previous = container.directories[path]
	if previous ~= nil and previous ~= instance then
		self:__forgetDirectory(container, path)
	end

	container.directories[path] = instance
	container.directoryPaths[instance] = path

	if container.directoryConnections[instance] == nil and self.__running then
		self:__connectDirectory(container, instance)
	end
end

function AssetSync:__forgetDirectory(container, path: string)
	local instance = container.directories[path]
	if instance == nil then
		return
	end

	container.directories[path] = nil
	if container.directoryPaths[instance] == path then
		container.directoryPaths[instance] = nil

		local connection = container.directoryConnections[instance]
		if connection ~= nil then
			connection:Disconnect()
			container.directoryConnections[instance] = nil
		end
	end
end

--[[
	Finds or creates the folder for the directory at `path`.
]]
function AssetSync:__ensureDirectory(container, path: string?): Instance?
	if path == nil then
		return container.instance
	end

	local known = container.directories[path]
	if known ~= nil and known.Parent ~= nil then
		return known
	end

	local parent = self:__ensureDirectory(container, Layout.parentPath(path))
	if parent == nil then
		return nil
	end

	local name = names.folderNameFromDir(string.match(path, "[^/]+$") :: string)
	if name == nil then
		return nil
	end

	-- Reuse a plain folder with the right name, for example one that was empty
	-- until now, rather than creating a duplicate next to it.
	local folder = nil
	for _, child in parent:GetChildren() do
		if
			child.ClassName == "Folder"
			and child.Name == name
			and container.directoryPaths[child] == nil
			and next(child:GetAttributes()) == nil
			and #child:GetTags() == 0
			and Layout.isSyncable(child, self.__instanceMap)
		then
			folder = child
			break
		end
	end

	if folder == nil then
		folder = Instance.new("Folder")
		folder.Name = name
		folder.Parent = parent
	end

	self:__setDirectory(container, path, folder)
	self:__markLayoutDirty(container, folder)
	return folder
end

--[[
	Applies files to Studio, and deletes the assets whose files were deleted.
	Paths in `changed` whose files no longer exist are deleted too.
]]
function AssetSync:__applyFileChanges(container, changed: { string }, removed: { string })
	local files = {}
	for start = 1, #changed, READ_BATCH_SIZE do
		local paths = table.move(changed, start, math.min(start + READ_BATCH_SIZE - 1, #changed), 1, {})
		local response = self.__apiContext
			:assetsRead({
				container = container.id,
				paths = paths,
			})
			:expect()

		for _, file in response.files do
			table.insert(files, file)
		end
	end

	local readPaths = {}
	for _, file in files do
		readPaths[file.path] = true
	end

	-- New instances are added as the last child, so adding them in suffix order
	-- keeps the order of siblings with the same name the same as the order of
	-- their `~N` suffixes, like `rojo build` does.
	local sortKeys = {}
	for _, file in files do
		sortKeys[file] = names.pathSortKey(file.path)
	end
	table.sort(files, function(a, b)
		return sortKeys[a] < sortKeys[b]
	end)

	local toRemove = table.clone(removed)
	for _, path in changed do
		if not readPaths[path] then
			table.insert(toRemove, path)
		end
	end

	-- Deserializing waits a frame per file, so all of it happens up front.
	local buffers = table.create(#files)
	for index, file in files do
		buffers[index] = file.data
	end
	local deserialized = serializer.deserializeMany(buffers)

	-- Adding and removing thousands of instances takes Studio a while, so this
	-- is spread over frames too. It's still a single step to undo.
	local recording = ChangeHistoryService:TryBeginRecording("RojoAssetSync", "Rojo: Sync assets from files")
	local applied = {}
	local removedPaths = {}
	local nextStep = frameBudget(FRAME_WORK_BUDGET)

	local success, err = pcall(function()
		for _, path in toRemove do
			nextStep()

			local entry = container.units[path]
			if entry ~= nil then
				self:__markLayoutDirty(container, entry.instance)
				self:__forgetUnit(container, path)
				container.dirtyUnits[entry.instance] = nil

				-- Undoing has to be able to bring this back, which it can't
				-- after Destroy.
				pcall(function()
					entry.instance.Parent = nil
				end)
			end
			table.insert(removedPaths, path)
		end

		for index, file in files do
			nextStep()

			local result = deserialized[index]
			local instances = result.instances

			if instances == nil or #instances ~= 1 then
				Log.warn(
					"Could not load asset {}: {}",
					file.path,
					if instances ~= nil then "it does not contain exactly one instance" else result.err
				)
				if instances ~= nil then
					for _, instance in instances do
						instance:Destroy()
					end
				end
				continue
			end

			local instance = instances[1]
			local parent = self:__ensureDirectory(container, Layout.parentPath(file.path))
			if parent == nil then
				Log.warn("Could not load asset {} because its directory can't be mapped to a folder", file.path)
				instance:Destroy()
				continue
			end

			-- The folder may have been deleted while this was waiting for the
			-- next frame.
			local placed, placeError = pcall(function()
				instance.Parent = parent
			end)
			if not placed then
				Log.warn("Could not load asset {}: {}", file.path, placeError)
				instance:Destroy()
				continue
			end

			local previous = container.units[file.path]
			if previous ~= nil and previous.instance ~= instance then
				self:__markLayoutDirty(container, previous.instance)
				container.dirtyUnits[previous.instance] = nil
				pcall(function()
					previous.instance.Parent = nil
				end)
			end

			self:__setUnit(container, file.path, instance, nil)
			self:__markLayoutDirty(container, instance)
			table.insert(applied, { path = file.path, instance = instance, fsHash = file.hash })
		end

		self:__pruneDirectories(container, removedPaths)
	end)

	if recording ~= nil then
		-- Adding to the undo history takes Studio most of a frame however
		-- little changed, so it gets a frame of its own.
		task.wait()
		ChangeHistoryService:FinishRecording(recording, Enum.FinishRecordingOperation.Commit)
		task.wait()
	end

	if not success then
		error(err)
	end

	-- Remember what Studio serializes the applied assets to, so that we know
	-- they're unchanged until someone edits them.
	local instances = {}
	for index, entry in applied do
		instances[index] = entry.instance
	end
	local results = serializer.serializeMany(instances, false)

	local acks = {}
	for index, entry in applied do
		local result = results[index]
		local current = container.units[entry.path]
		if result.hash ~= nil and current ~= nil and current.instance == entry.instance then
			current.hash = result.hash
			container.dirtyUnits[entry.instance] = nil
			table.insert(acks, {
				path = entry.path,
				fsHash = entry.fsHash,
				studioHash = buffer.fromstring(result.hash),
			})
		end
	end

	if #acks > 0 or #removedPaths > 0 then
		self.__apiContext
			:assetsAck({
				placeId = self.__placeId,
				container = container.id,
				applied = acks,
				removed = removedPaths,
			})
			:expect()
	end

	return #applied, #removedPaths
end

--[[
	Deletes folders that no longer contain anything after the assets at the
	given paths were deleted, since their directories are gone too.
]]
function AssetSync:__pruneDirectories(container, removedPaths: { string })
	local candidates = {}
	for _, path in removedPaths do
		local parent = Layout.parentPath(path)
		while parent ~= nil do
			candidates[parent] = true
			parent = Layout.parentPath(parent)
		end
	end

	local sorted = {}
	for path in candidates do
		table.insert(sorted, path)
	end
	-- Deepest first, so parents are checked after their children are gone.
	table.sort(sorted, function(a, b)
		return #a > #b
	end)

	for _, path in sorted do
		local folder = container.directories[path]
		if folder == nil then
			continue
		end

		local hasSyncableChildren = false
		for _, child in folder:GetChildren() do
			if Layout.isSyncable(child, self.__instanceMap) then
				hasSyncableChildren = true
				break
			end
		end

		if not hasSyncableChildren then
			self:__markLayoutDirty(container, folder)
			self:__forgetDirectory(container, path)
			pcall(function()
				folder.Parent = nil
			end)
		end
	end
end

--[[
	Finds the container and the asset an instance belongs to.

	Returns the container and the unit the instance is part of. If the
	instance is part of a container but not of a known unit, it's a new unit
	or folder, and only the container is returned.
]]
function AssetSync:__locate(instance: Instance)
	local current = instance

	while current ~= nil do
		local container = self.__containerByInstance[current]
		if container ~= nil then
			return container, nil
		end

		for _, candidate in self.__containers do
			if candidate.unitPaths[current] ~= nil then
				return candidate, current
			end
		end

		-- Rojo syncs these from the project, so they aren't assets.
		if self.__instanceMap.fromInstances[current] ~= nil then
			return nil, nil
		end

		current = current.Parent
	end

	return nil, nil
end

--[[
	Like __locate, but reuses what was found for the instance's parent earlier
	in the same frame. Adding a model fires an event for every one of its
	descendants, and walking up from each of them adds up when there are
	thousands.
]]
function AssetSync:__locateCached(instance: Instance)
	local parent = instance.Parent
	local cached = if parent ~= nil then self.__located[parent] else nil

	local container, unit
	if cached == nil then
		container, unit = self:__locate(instance)
	elseif cached.unit ~= nil then
		-- Everything in a unit belongs to it.
		container, unit = cached.container, cached.unit
	elseif cached.container ~= nil and cached.container.unitPaths[instance] ~= nil then
		container, unit = cached.container, instance
	else
		container, unit = cached.container, nil
	end

	self.__located[instance] = { container = container, unit = unit }
	return container, unit
end

function AssetSync:__markUnitDirty(container, unit: Instance)
	local now = os.clock()
	local dirty = container.dirtyUnits[unit]
	if dirty == nil then
		container.dirtyUnits[unit] = { first = now, last = now }
	else
		dirty.last = now
	end
end

function AssetSync:__markStructureDirty(container)
	if container.structureDirty == nil then
		container.structureDirty = os.clock()
	end
end

--[[
	Marks the directories whose entries an instance affects as changed, so that
	only those are looked at when the layout is recomputed: the directory it's
	in, the one it was in before if it moved, and the directory above each of
	those, since whether a folder is a directory depends on what's in it.
]]
function AssetSync:__markLayoutDirty(container, instance: Instance)
	local function mark(directory: Instance?)
		if directory == nil then
			return
		end

		container.dirtyDirectories[directory] = true
		if directory ~= container.instance and directory.Parent ~= nil then
			container.dirtyDirectories[directory.Parent] = true
		end
	end

	mark(instance.Parent)

	-- Deferred events can fire after the instance was already removed, so
	-- where it was is taken from what we know.
	local path = container.unitPaths[instance] or container.directoryPaths[instance]
	if path ~= nil then
		local parentPath = Layout.parentPath(path)
		mark(if parentPath == nil then container.instance else container.directories[parentPath])
	end

	self:__markStructureDirty(container)
end

--[[
	Marks whatever an instance belongs to as changed. `property` is nil for
	changes other than to a property, like being added or tagged.
]]
function AssetSync:__onInstanceChanged(instance: Instance, property: string?)
	if not self.__running then
		return
	end

	-- ItemChanged fires for every instance in the game, including ones whose
	-- ancestry plugins aren't allowed to read.
	local located, container, unit = pcall(self.__locateCached, self, instance)
	if not located or container == nil then
		return
	end

	-- Whether this can change where an asset is stored, or whether it's synced.
	local affectsLayout = property == nil or property == "Name" or property == "Parent" or property == "Archivable"

	if unit ~= nil then
		self:__markUnitDirty(container, unit)

		-- A unit's file name depends on its name and where it is, and a folder
		-- stored as a file becomes a directory once it has something in it to
		-- sync.
		local changesUnit = unit == instance
		local changesFolder = property ~= "Name" and unit.ClassName == "Folder" and instance.Parent == unit
		if affectsLayout and (changesUnit or changesFolder) then
			self:__markLayoutDirty(container, unit)
		end
	elseif affectsLayout and instance ~= container.instance then
		-- Folders and anything new in them affect the layout.
		self:__markLayoutDirty(container, instance)
	end
end

function AssetSync:__connectUnit(container, instance: Instance)
	container.unitConnections[instance] = {
		-- Descendants that are removed can't be traced back to their unit
		-- anymore by the time a deferred event fires, so every unit listens
		-- for its own.
		instance.DescendantRemoving:Connect(function()
			if self.__running then
				self:__markUnitDirty(container, instance)
			end
		end),
		-- Studio doesn't fire Changed for attributes.
		instance.AttributeChanged:Connect(function()
			if self.__running then
				self:__markUnitDirty(container, instance)

				-- A folder without attributes may be stored as a directory.
				if instance.ClassName == "Folder" then
					self:__markLayoutDirty(container, instance)
				end
			end
		end),
	}
end

function AssetSync:__connectDirectory(container, instance: Instance)
	-- A folder with attributes is stored as a file instead of a directory.
	container.directoryConnections[instance] = instance.AttributeChanged:Connect(function()
		if self.__running then
			self:__markLayoutDirty(container, instance)
		end
	end)
end

function AssetSync:__connectListeners()
	-- ItemChanged fires for every property change of every instance in the
	-- game, which is far cheaper than connecting to Changed of every one of
	-- possibly hundreds of thousands of assets.
	-- selene: allow(deprecated)
	local listening = pcall(function()
		table.insert(
			self.__connections,
			game.ItemChanged:Connect(function(instance, property)
				self:__onInstanceChanged(instance, property)
			end)
		)
	end)

	if not listening then
		-- Edits made in Studio are almost always made to the selection, so
		-- listening to the selection catches those right away, and checking
		-- every asset more often catches the rest.
		Log.debug("ItemChanged is unavailable, so asset changes are found by checking more often")
		self.__watchSelectionChanges = true
		self.__sweepPeriod = SWEEP_PERIOD / 4
		self.__layoutSweepPeriod = LAYOUT_SWEEP_PERIOD / 4
	end

	-- The game can change between frames, so what was found where is only
	-- remembered for a frame.
	table.insert(
		self.__connections,
		RunService.Heartbeat:Connect(function()
			if next(self.__located) ~= nil then
				table.clear(self.__located)
			end
		end)
	)

	for _, container in self.__containers do
		table.insert(
			self.__connections,
			container.instance.DescendantAdded:Connect(function(instance)
				self:__onInstanceChanged(instance, nil)
			end)
		)

		table.insert(
			self.__connections,
			container.instance.DescendantRemoving:Connect(function(instance)
				if not self.__running then
					return
				end

				-- Only removed units and folders change the layout. Anything
				-- else is caught by its unit's own DescendantRemoving.
				if container.unitPaths[instance] ~= nil or container.directoryPaths[instance] ~= nil then
					self:__markLayoutDirty(container, instance)
				end
			end)
		)

		for _, entry in container.units do
			if container.unitConnections[entry.instance] == nil then
				self:__connectUnit(container, entry.instance)
			end
		end

		for _, instance in container.directories do
			if container.directoryConnections[instance] == nil then
				self:__connectDirectory(container, instance)
			end
		end
	end

	-- Tags don't fire ItemChanged either, but CollectionService can tell us
	-- about every tag that's added or removed.
	local connectedTags = {}
	local function connectTag(tag: string)
		-- A tag that stops being used and is used again fires TagAdded again,
		-- while its signals are still connected.
		if connectedTags[tag] then
			return
		end
		connectedTags[tag] = true

		table.insert(
			self.__connections,
			CollectionService:GetInstanceAddedSignal(tag):Connect(function(instance)
				self:__onInstanceChanged(instance, nil)
			end)
		)
		table.insert(
			self.__connections,
			CollectionService:GetInstanceRemovedSignal(tag):Connect(function(instance)
				self:__onInstanceChanged(instance, nil)
			end)
		)
	end

	for _, tag in CollectionService:GetAllTags() do
		connectTag(tag)
	end
	table.insert(
		self.__connections,
		CollectionService.TagAdded:Connect(function(tag)
			connectTag(tag)

			-- TagAdded only fires for a tag the game didn't use yet once
			-- something has it, which is too late for the signals connected
			-- above to report that.
			for _, instance in CollectionService:GetTagged(tag) do
				self:__onInstanceChanged(instance, nil)
			end
		end)
	)

	-- Attributes of an asset's descendants are almost always edited in the
	-- Properties window, which edits the selection, so listening to the
	-- selection catches those without listening to every instance. The sweep
	-- catches everything else.
	table.insert(
		self.__connections,
		Selection.SelectionChanged:Connect(function()
			self:__onSelectionChanged()
		end)
	)
	self:__onSelectionChanged()
end

function AssetSync:__onSelectionChanged()
	for _, connection in self.__selectionConnections do
		connection:Disconnect()
	end
	table.clear(self.__selectionConnections)

	if not self.__running then
		return
	end

	for index, instance in Selection:Get() do
		if index > 1000 then
			break
		end

		local located, container = pcall(self.__locate, self, instance)
		if located and container ~= nil then
			table.insert(
				self.__selectionConnections,
				instance.AttributeChanged:Connect(function()
					self:__onInstanceChanged(instance, nil)
				end)
			)

			if self.__watchSelectionChanges then
				table.insert(
					self.__selectionConnections,
					instance.Changed:Connect(function(property)
						self:__onInstanceChanged(instance, property)
					end)
				)
			end
		end
	end
end

function AssetSync:__onSocketPacket(packet)
	if not self.__running then
		return
	end

	if packet.containersChanged then
		Log.warn("The project's asset directories changed. Reconnect to sync them.")
		self.__notify("The project's asset directories changed. Reconnect to sync them.")
	end

	for _, change in packet.changes do
		if change.origin ~= self.__clientId then
			if #self.__fsChanges == 0 then
				self.__firstFsChangeAt = os.clock()
			end
			self.__lastFsChangeAt = os.clock()
			table.insert(self.__fsChanges, change)
		end
	end
end

--[[
	Whether queued file changes should be applied now. Tools like git change
	many files in a burst, and applying the whole burst at once is much faster
	than applying it piece by piece as it arrives.
]]
function AssetSync:__fileChangesAreDue(now: number): boolean
	if #self.__fsChanges == 0 then
		return false
	end

	return now - self.__lastFsChangeAt >= FILE_CHANGE_QUIET_PERIOD
		or now - self.__firstFsChangeAt >= MAX_FILE_CHANGE_DELAY
end

--[[
	Applies queued file changes to Studio.
]]
function AssetSync:__processFileChanges()
	local changes = self.__fsChanges
	self.__fsChanges = {}

	-- Only the latest change to each path matters.
	local latest = {}
	for _, change in changes do
		local container = self.__containerById[change.container]
		if container == nil then
			continue
		end

		latest[container] = latest[container] or {}
		latest[container][change.path] = change.removed
	end

	for container, paths in latest do
		local changed, removed = {}, {}
		for path, wasRemoved in paths do
			if container.dirtyUnits[container.units[path] and container.units[path].instance] then
				Log.warn("{} changed on disk while it had unsaved changes in Studio. Using the file's version.", path)
			end

			if wasRemoved then
				table.insert(removed, path)
			else
				table.insert(changed, path)
			end
		end

		local appliedCount, removedCount = self:__applyFileChanges(container, changed, removed)
		if appliedCount + removedCount > 0 then
			Log.info(
				"Applied {} changed and {} deleted asset files to {}",
				appliedCount,
				removedCount,
				container.instance:GetFullName()
			)
		end
	end
end

--[[
	Whether a container has changes that are ready to be written.
]]
function AssetSync:__hasDueChanges(container, now: number): boolean
	if container.retryAt ~= nil and now < container.retryAt then
		return false
	end

	if next(container.pendingRemovals) ~= nil then
		return true
	end

	if container.structureDirty ~= nil and now - container.structureDirty >= QUIET_PERIOD then
		return true
	end

	for _, dirty in container.dirtyUnits do
		if now - dirty.last >= QUIET_PERIOD or now - dirty.first >= MAX_DIRTY_AGE then
			return true
		end
	end

	return false
end

local function isUnder(path: string, directory: string): boolean
	return string.sub(path, 1, #directory + 1) == directory .. "/"
end

local function depthOf(path: string): number
	local _, slashes = string.gsub(path, "/", "")
	return slashes
end

--[[
	Finds directories that can be moved as a whole because everything in them
	moved along with them, like the directory of a renamed folder. Moving a
	directory takes a single rename however much is in it. Returns a map of
	old directory paths to new ones.

	`moves` maps the new paths of assets whose files are moved to their old
	paths. A directory is only worth moving if it holds at least one of them.
]]
function AssetSync:__findDirectoryMoves(container, targetUnits, targetDirectories, moves)
	local targetPaths = {}
	for path, instance in targetUnits do
		targetPaths[instance] = path
	end

	local candidates = {}
	for newPath, folder in targetDirectories do
		local oldPath = container.directoryPaths[folder]
		if
			oldPath ~= nil
			and oldPath ~= newPath
			and not isUnder(newPath, oldPath)
			and not isUnder(oldPath, newPath)
		then
			table.insert(candidates, { from = oldPath, to = newPath })
		end
	end

	-- Directories inside of one that's moved as a whole move along with it.
	table.sort(candidates, function(a, b)
		return depthOf(a.from) < depthOf(b.from)
	end)

	local directoryMoves = {}
	for _, candidate in candidates do
		local covered = false
		for from in directoryMoves do
			if isUnder(candidate.from, from) then
				covered = true
				break
			end
		end
		if covered then
			continue
		end

		local whole, worthIt = true, false
		for path, entry in container.units do
			if isUnder(path, candidate.from) then
				local target = targetPaths[entry.instance]
				if target ~= candidate.to .. string.sub(path, #candidate.from + 1) then
					whole = false
					break
				end
				worthIt = worthIt or moves[target] == path
			elseif isUnder(path, candidate.to) and targetUnits[path] == entry.instance then
				-- Something stays where the directory would go.
				whole = false
				break
			end
		end

		if whole and worthIt then
			directoryMoves[candidate.from] = candidate.to
		end
	end

	return directoryMoves
end

--[[
	Writes changes made in Studio to files.
]]
function AssetSync:__reconcile(container)
	local now = os.clock()
	local structureChanged = container.structureDirty ~= nil
	local structureSnapshot = container.structureDirty

	local targetUnits, targetDirectories
	if structureChanged then
		-- Only directories marked dirty since the last layout are looked at.
		local dirty = container.dirtyDirectories
		container.dirtyDirectories = {}

		local computed, layout = pcall(
			Layout.compute,
			container.instance,
			self.__instanceMap,
			container.unitPaths,
			container.directoryPaths,
			nil,
			container.layoutEntries,
			dirty
		)
		if not computed then
			for directory in dirty do
				container.dirtyDirectories[directory] = true
			end
			error(layout, 0)
		end

		container.layoutEntries = layout.entries
		targetUnits, targetDirectories = layout.units, layout.directories
	else
		targetUnits, targetDirectories = {}, container.directories
		for path, entry in container.units do
			targetUnits[path] = entry.instance
		end
	end

	-- Changes that arrive while we're writing must not be forgotten, so only
	-- changes that are due now are handled.
	local dueUnits = {}
	for unit, dirty in container.dirtyUnits do
		if now - dirty.last >= QUIET_PERIOD or now - dirty.first >= MAX_DIRTY_AGE then
			dueUnits[unit] = dirty
		end
	end

	-- Assets that are stored in another file now, like everything in a folder
	-- that was renamed, have their files moved rather than written again,
	-- unless they changed as well. That keeps renaming or moving a folder cheap
	-- however much is in it. Maps new paths to old ones.
	local moves = {}
	for path, instance in targetUnits do
		local oldPath = container.unitPaths[instance]
		local old = if oldPath ~= nil then container.units[oldPath] else nil
		if oldPath ~= path and old ~= nil and old.instance == instance and old.hash ~= nil then
			moves[path] = oldPath
		end
	end

	local toSerialize = {}
	local toSerializePaths = {}
	for path, instance in targetUnits do
		local entry = container.units[path]
		local isNew = entry == nil or entry.instance ~= instance
		if dueUnits[instance] ~= nil or (isNew and moves[path] == nil) then
			table.insert(toSerialize, instance)
			table.insert(toSerializePaths, path)
		end
	end

	local results = serializer.serializeMany(toSerialize, true)

	local writes = {}
	local newHashes = {}
	for index, path in toSerializePaths do
		local result = results[index]
		local instance = toSerialize[index]
		local entry = container.units[path]

		if result.hash == nil then
			Log.warn("Could not serialize asset {}: {}", instance:GetFullName(), result.err)
			continue
		end

		local oldPath = moves[path]
		if oldPath ~= nil and container.units[oldPath].hash ~= result.hash then
			moves[path] = nil
			oldPath = nil
		end

		local unchanged = oldPath ~= nil or (entry ~= nil and entry.instance == instance and entry.hash == result.hash)
		if not unchanged then
			writes[path] = result.data
		end
		newHashes[path] = result.hash
	end

	-- Moved files keep their hashes.
	local movedFrom = {}
	for path, oldPath in moves do
		movedFrom[oldPath] = path
		if newHashes[path] == nil then
			newHashes[path] = container.units[oldPath].hash
		end
	end

	-- Paths that something else is written to are overwritten rather than
	-- deleted, so a failed write never leaves the path without a file. Moves
	-- don't overwrite anything though, so whatever is in their way is removed
	-- first.
	local removals = {}
	for path in container.units do
		if movedFrom[path] == nil and (targetUnits[path] == nil or moves[path] ~= nil) then
			removals[path] = true
		end
	end
	for path in container.pendingRemovals do
		if movedFrom[path] == nil and (targetUnits[path] == nil or moves[path] ~= nil) then
			removals[path] = true
		end
	end
	table.clear(container.pendingRemovals)

	local directoryMoves = if structureChanged
		then self:__findDirectoryMoves(container, targetUnits, targetDirectories, moves)
		else {}

	-- Files in directories that are moved as a whole are moved along with
	-- them. Maps their new paths to the directory's old path.
	local movedWith = {}
	if next(directoryMoves) ~= nil then
		for path, oldPath in moves do
			local directory = Layout.parentPath(oldPath)
			while directory ~= nil do
				if directoryMoves[directory] ~= nil then
					movedWith[path] = directory
					break
				end
				directory = Layout.parentPath(directory)
			end
		end
	end

	local ops = {}
	for path in removals do
		table.insert(ops, { kind = "remove", path = path })
	end
	for from, to in directoryMoves do
		table.insert(ops, { kind = "move", path = from, to = to })
	end
	for path, oldPath in moves do
		if movedWith[path] == nil then
			table.insert(ops, { kind = "move", path = oldPath, to = path })
		end
	end
	for path, data in writes do
		table.insert(ops, { kind = "write", path = path, data = data })
	end

	local failures = {}
	if #ops > 0 then
		local response = self.__apiContext
			:assetsWrite({
				clientId = self.__clientId,
				placeId = self.__placeId,
				container = container.id,
				ops = ops,
			})
			:expect()

		for _, failure in response.failures do
			failures[failure.path] = failure.error
		end

		-- Whatever made them fail, like another program having a file open,
		-- usually takes a moment to go away.
		if #response.failures > 0 then
			container.retryAt = os.clock() + RETRY_DELAY
		end

		local counts = { write = 0, move = 0, remove = 0 }
		for _, op in ops do
			if failures[op.path] == nil then
				counts[op.kind] += 1
			end
		end
		Log.debug(
			"Wrote {}, moved {}, and deleted {} asset files in {}",
			counts.write,
			counts.move,
			counts.remove,
			container.path
		)
	end

	-- Everything worked out, apart from the failures, so update what we know.
	for path in removals do
		if failures[path] ~= nil then
			container.pendingRemovals[path] = true
		end
		self:__forgetUnit(container, path)
	end

	-- A move that failed left the file where it was, or it's gone. Either way,
	-- writing the asset again and removing its old file sorts it out.
	local failedMoves = {}
	for path, oldPath in moves do
		local directory = movedWith[path]
		if failures[oldPath] ~= nil or (directory ~= nil and failures[directory] ~= nil) then
			failedMoves[path] = true
			container.pendingRemovals[oldPath] = true
		end
	end

	for path, instance in targetUnits do
		local hash = newHashes[path]
		if failures[path] ~= nil or failedMoves[path] then
			-- Without a hash, the asset is written again next time.
			hash = nil
			self:__markUnitDirty(container, instance)
		elseif hash == nil then
			local entry = container.units[path]
			hash = if entry ~= nil and entry.instance == instance then entry.hash else nil
		end

		self:__setUnit(container, path, instance, hash)
	end

	if structureChanged then
		for path in table.clone(container.directories) do
			if targetDirectories[path] == nil then
				self:__forgetDirectory(container, path)
			end
		end
		for path, instance in targetDirectories do
			self:__setDirectory(container, path, instance)
		end

		if container.structureDirty == structureSnapshot then
			container.structureDirty = nil
		end
	end

	for unit, dirty in dueUnits do
		-- Units that changed again while we were writing stay dirty.
		if container.dirtyUnits[unit] == dirty and dirty.last < now then
			container.dirtyUnits[unit] = nil
		end
	end

	-- Forget about changes to units that aren't assets anymore.
	for unit in container.dirtyUnits do
		if container.unitPaths[unit] == nil then
			container.dirtyUnits[unit] = nil
		end
	end
end

--[[
	Re-checks a few assets for changes that no event reported, paced so that
	every asset gets checked once per SWEEP_PERIOD.
]]
function AssetSync:__sweepStep(interval: number)
	if #self.__sweepQueue == 0 then
		for _, container in self.__containers do
			for path in container.units do
				table.insert(self.__sweepQueue, { container = container, path = path })
			end
		end
		self.__sweepTotal = #self.__sweepQueue
	end

	-- Fractions of an asset carry over to the next step. Rounding up instead
	-- would check one asset every step, so a container with only a few assets
	-- would be checked over and over.
	self.__sweepBudget += (self.__sweepTotal or 0) * interval / (self.__sweepPeriod or SWEEP_PERIOD)
	local count = math.floor(self.__sweepBudget)
	self.__sweepBudget -= count

	local items, instances = {}, {}

	while #items < count and #self.__sweepQueue > 0 do
		local item = table.remove(self.__sweepQueue)
		local entry = item.container.units[item.path]

		if entry ~= nil and entry.hash ~= nil and item.container.dirtyUnits[entry.instance] == nil then
			item.entry = entry
			table.insert(items, item)
			table.insert(instances, entry.instance)
		end
	end

	if #items == 0 then
		return
	end

	local results = serializer.serializeMany(instances, false)

	for index, item in items do
		local result = results[index]

		-- The asset may have been replaced or written while it was being
		-- serialized, in which case its hash is already up to date.
		if item.container.units[item.path] == item.entry and result.hash ~= nil and result.hash ~= item.entry.hash then
			Log.debug("Found unreported changes to asset {}", item.path)
			self:__markUnitDirty(item.container, item.entry.instance)
		end
	end
end

--[[
	Re-checks a few directories for changes to the layout that no event
	reported, paced so that every directory gets checked once per
	LAYOUT_SWEEP_PERIOD. Only directories whose entries changed are marked
	dirty, so the layout isn't recomputed when nothing changed.
]]
function AssetSync:__layoutSweepStep(interval: number)
	if #self.__layoutSweepQueue == 0 then
		for _, container in self.__containers do
			for directory in container.layoutEntries do
				table.insert(self.__layoutSweepQueue, { container = container, directory = directory })
			end
		end
		self.__layoutSweepTotal = #self.__layoutSweepQueue
	end

	-- Fractions carry over, like in __sweepStep.
	local period = self.__layoutSweepPeriod or LAYOUT_SWEEP_PERIOD
	self.__layoutSweepBudget += (self.__layoutSweepTotal or 0) * interval / period
	local count = math.floor(self.__layoutSweepBudget)
	self.__layoutSweepBudget -= count

	for _ = 1, count do
		local item = table.remove(self.__layoutSweepQueue)
		if item == nil then
			break
		end

		local container, directory = item.container, item.directory
		local entries = container.layoutEntries[directory]
		local path = if directory == container.instance then nil else container.directoryPaths[directory]

		if entries ~= nil and (path ~= nil or directory == container.instance) then
			local current = Layout.entriesOf(
				container.instance,
				directory,
				path,
				self.__instanceMap,
				container.unitPaths,
				container.directoryPaths
			)

			if not Layout.sameEntries(current, entries) then
				Log.debug("Found unreported changes to the layout of {}", path or container.path)
				container.dirtyDirectories[directory] = true
				self:__markStructureDirty(container)
			end
		end
	end
end

function AssetSync:__workerLoop()
	local lastSweep = os.clock()

	while self.__running do
		local now = os.clock()
		local success, err = true, nil

		if now >= self.__retryAt then
			if self:__fileChangesAreDue(now) then
				success, err = pcall(self.__processFileChanges, self)
			elseif #self.__fsChanges > 0 then
				-- Wait for the rest of the burst before doing anything else, so
				-- Studio changes aren't written on top of files being changed.
				task.wait(0.02)
				continue
			else
				local worked = false
				for _, container in self.__containers do
					if self:__hasDueChanges(container, now) then
						worked = true
						success, err = pcall(self.__reconcile, self, container)
						if not success then
							break
						end
					end
				end

				-- Only sweep when there's nothing else to do, so it never
				-- delays real changes.
				if not worked then
					success, err = pcall(self.__sweepStep, self, now - lastSweep)
					if success then
						success, err = pcall(self.__layoutSweepStep, self, now - lastSweep)
					end
					lastSweep = now
				end
			end

			if not success and self.__running then
				Log.warn("Asset sync failed, retrying in {} seconds: {}", RETRY_DELAY, errorToString(err))
				self.__retryAt = os.clock() + RETRY_DELAY
			end
		end

		-- Always yield, so that a bug that keeps reporting work can never
		-- freeze Studio.
		task.wait(0.1)
	end
end

return AssetSync
