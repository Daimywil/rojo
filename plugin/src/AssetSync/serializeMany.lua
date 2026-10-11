--[[
	Serializes and deserializes many instances, spread over as many frames as
	it takes to keep Studio responsive.

	SerializationService does its work on Studio's main thread however it's
	called, so running hundreds of calls at once makes Studio stutter. Running
	them one at a time is slow instead, since every call takes at least a frame.
	How many run at once adapts to how long frames take: it goes up while frames
	stay short and halves as soon as one takes noticeably longer than usual.

	The functions that serialize or deserialize take an optional `onProgress`,
	which is called with how many are done after every frame.
]]

local EncodingService = game:GetService("EncodingService")
local SerializationService = game:GetService("SerializationService")

-- How much longer than usual a frame may take because of work spread over it.
local FRAME_BUDGET = 0.012
-- How many calls run at once to start with, and at most. Every batch starts
-- small, since how long a call takes depends on what's being serialized.
local INITIAL_CONCURRENCY = 4
local MAX_CONCURRENCY = 64
-- How long a measurement of Studio's usual frame time is trusted for.
local USUAL_FRAME_LIFETIME = 10

local usualFrameTime = 0
local usualFrameMeasuredAt = -math.huge

export type Result = {
	-- The serialized instances, if keepData was set.
	data: buffer?,
	-- The BLAKE3 hash of the serialized instances, as a 32 byte string.
	hash: string?,
	-- Why serializing failed, if it did.
	err: string?,
}

export type DeserializeResult = {
	instances: { Instance }?,
	err: string?,
}

local function measureUsualFrameTime(): number
	if os.clock() - usualFrameMeasuredAt > USUAL_FRAME_LIFETIME then
		local shortest = math.huge
		for _ = 1, 3 do
			shortest = math.min(shortest, task.wait())
		end
		usualFrameTime = shortest
		usualFrameMeasuredAt = os.clock()
	end

	return usualFrameTime
end

--[[
	Calls `job` with every index from 1 to `count`, spread over frames. Calls
	`onProgress`, if given, with how many jobs finished after every frame.
]]
local function spread(count: number, job: (number) -> (), onProgress: ((number) -> ())?)
	if count == 0 then
		return
	end

	local longestFrame = measureUsualFrameTime() + FRAME_BUDGET
	local concurrency = INITIAL_CONCURRENCY
	local inFlight, finished, nextIndex = 0, 0, 1

	while finished < count do
		local frameStarted = os.clock()

		while nextIndex <= count and inFlight < concurrency do
			local index = nextIndex
			nextIndex += 1
			inFlight += 1

			task.spawn(function()
				job(index)
				inFlight -= 1
				finished += 1
			end)

			-- Some calls, like deserializing, finish without yielding, so the
			-- time they take counts toward this frame.
			if os.clock() - frameStarted > FRAME_BUDGET then
				break
			end
		end

		local frameTime = task.wait()
		if frameTime > longestFrame then
			concurrency = math.max(1, concurrency // 2)
		elseif inFlight >= concurrency then
			concurrency = math.min(MAX_CONCURRENCY, concurrency + 2)
		end

		if onProgress ~= nil then
			onProgress(finished)
		end
	end
end

local function hashBuffer(data: buffer): string
	return buffer.tostring(EncodingService:ComputeBufferHash(data, Enum.HashAlgorithm.Blake3))
end

local function serializeGroup(instances: { Instance }, keepData: boolean): Result
	local success, data = pcall(SerializationService.SerializeInstancesAsync, SerializationService, instances)
	if not success then
		return { err = tostring(data) }
	end

	return {
		data = if keepData then data else nil,
		hash = hashBuffer(data),
	}
end

--[[
	Serializes each instance on its own. Results are returned in the same
	order as the given instances.
]]
local function serializeMany(instances: { Instance }, keepData: boolean, onProgress: ((number) -> ())?): { Result }
	local results = table.create(#instances)
	spread(#instances, function(index)
		results[index] = serializeGroup({ instances[index] }, keepData)
	end, onProgress)
	return results
end

--[[
	Serializes each group of instances together, which is much faster than
	serializing them one by one. Only the hashes are kept.
]]
local function serializeGroups(groups: { { Instance } }, onProgress: ((number) -> ())?): { Result }
	local results = table.create(#groups)
	spread(#groups, function(index)
		results[index] = serializeGroup(groups[index], false)
	end, onProgress)
	return results
end

--[[
	Deserializes many models. Results are returned in the same order as the
	given buffers.
]]
local function deserializeMany(buffers: { buffer }, onProgress: ((number) -> ())?): { DeserializeResult }
	local results = table.create(#buffers)
	spread(#buffers, function(index)
		local success, instances =
			pcall(SerializationService.DeserializeInstancesAsync, SerializationService, buffers[index])
		results[index] = if success then { instances = instances } else { err = tostring(instances) }
	end, onProgress)
	return results
end

return {
	serializeMany = serializeMany,
	serializeGroups = serializeGroups,
	deserializeMany = deserializeMany,
	hashBuffer = hashBuffer,
}
