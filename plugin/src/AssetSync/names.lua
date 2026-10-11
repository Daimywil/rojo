--[[
	Mapping between Roblox instance names and asset file names.

	Roblox allows any string as an instance name, including characters that are
	invalid in file names and siblings that share a name. Names are encoded so
	that every instance gets a unique, valid file name:

	- Characters that are invalid on common filesystems, control characters,
	  `%`, and `~` are percent-encoded byte by byte (`:` becomes `%3A`).
	- A leading `.` or space and a trailing `.` or space are percent-encoded.
	- Names reserved by Windows (`CON`, `nul.txt`, ...) get their first
	  character percent-encoded.
	- Siblings whose file names collide on a case-insensitive filesystem get a
	  `~2`, `~3`, ... suffix in sibling order.

	This mirrors src/assets/names.rs exactly. Both are tested against the same
	cases, so any change here must be mirrored there.
]]

local UNIT_EXTENSION = ".rbxm"
local MAX_DIRECTORY_NAME_LEN = 200
local MAX_UNIT_STEM_LEN = 200

local RESERVED_NAMES = {}
for _, name in { "CON", "PRN", "AUX", "NUL" } do
	RESERVED_NAMES[name] = true
end
for i = 0, 9 do
	RESERVED_NAMES["COM" .. i] = true
	RESERVED_NAMES["LPT" .. i] = true
end
for _, superscript in { "¹", "²", "³" } do
	RESERVED_NAMES["COM" .. superscript] = true
	RESERVED_NAMES["LPT" .. superscript] = true
end

local ALWAYS_ESCAPED = {}
for _, character in { "<", ">", ":", '"', "/", "\\", "|", "?", "*", "%", "~" } do
	ALWAYS_ESCAPED[string.byte(character)] = true
end

local DOT = string.byte(".")
local SPACE = string.byte(" ")
local PERCENT = string.byte("%")

-- How many results a memoized function keeps before starting over.
local MAX_MEMOIZED = 50000

--[[
	Remembers the results of a function of a string. Layouts encode the same
	names over and over, like thousands of parts named "Part".
]]
local function memoize(compute: (string) -> string): (string) -> string
	local cache = {}
	local size = 0

	return function(input: string): string
		local result = cache[input]
		if result == nil then
			result = compute(input)
			if size >= MAX_MEMOIZED then
				table.clear(cache)
				size = 0
			end
			cache[input] = result
			size += 1
		end
		return result
	end
end

local function escapeBytes(bytes: string): string
	return (string.gsub(bytes, ".", function(byte)
		return string.format("%%%02X", string.byte(byte))
	end))
end

local function mustEscape(codepoint: number): boolean
	return ALWAYS_ESCAPED[codepoint] == true or codepoint <= 0x1F or (codepoint >= 0x7F and codepoint <= 0x9F)
end

local function isReserved(encoded: string): boolean
	local base = string.match(encoded, "^[^%.]*")
	base = string.gsub(base, " +$", "")
	return RESERVED_NAMES[string.upper(base)] == true
end

--[[
	Percent-encodes an instance name so that it is a valid file name on every
	common filesystem. Different names never encode to the same string.
]]
local encodeName = memoize(function(name: string): string
	local parts = {}

	if utf8.len(name) == nil then
		-- Names that aren't valid UTF-8 can't be decoded on the other side, so
		-- every byte outside of ASCII is escaped to keep the result valid.
		for index = 1, #name do
			local byte = string.byte(name, index)
			if byte >= 0x80 or mustEscape(byte) then
				table.insert(parts, string.format("%%%02X", byte))
			else
				table.insert(parts, string.char(byte))
			end
		end
	else
		local codepoints = {}
		for _, codepoint in utf8.codes(name) do
			table.insert(codepoints, codepoint)
		end

		for index, codepoint in codepoints do
			local isEdge = index == 1 or index == #codepoints
			local character = utf8.char(codepoint)

			if mustEscape(codepoint) or (isEdge and (codepoint == DOT or codepoint == SPACE)) then
				table.insert(parts, escapeBytes(character))
			else
				table.insert(parts, character)
			end
		end
	end

	local output = table.concat(parts)

	if isReserved(output) then
		output = string.format("%%%02X", string.byte(output, 1)) .. string.sub(output, 2)
	end

	return output
end)

--[[
	Reverses encodeName. Returns nil for strings it couldn't have produced.
]]
local function decodeName(encoded: string): string?
	if string.find(encoded, "~", 1, true) then
		return nil
	end

	local valid = true
	local decoded = string.gsub(encoded, "%%(..)", function(hex)
		if not string.match(hex, "^%x%x$") then
			valid = false
			return ""
		end
		return string.char(tonumber(hex, 16) :: number)
	end)

	-- Re-encoding rejects everything that isn't in canonical form, like
	-- lowercase hex, needless escapes, or a truncated escape sequence.
	if not valid or utf8.len(decoded) == nil or encodeName(decoded) ~= encoded then
		return nil
	end

	return decoded
end

--[[
	Returns the name of the folder a directory with the given name represents.
]]
local function folderNameFromDir(dirName: string): string?
	local base, suffix = string.match(dirName, "^(.*)~(%d+)$")
	if base ~= nil then
		local number = tonumber(suffix)
		if number == nil or number < 2 or tostring(number) ~= suffix then
			base = dirName
		end
	else
		base = dirName
	end

	local name = decodeName(base)
	if name == nil or name == "" then
		return nil
	end

	return name
end

--[[
	Whether a folder with the given name can be represented as a directory.
]]
local function canBeDirectory(name: string): boolean
	return name ~= "" and utf8.len(name) ~= nil and #encodeName(name) <= MAX_DIRECTORY_NAME_LEN
end

local function isContinuationByte(byte: number?): boolean
	return byte ~= nil and byte >= 0x80 and byte < 0xC0
end

local unitStem = memoize(function(name: string): string
	local stem = encodeName(name)

	if stem == "" then
		-- Units store their real name inside of the file, so this doesn't need
		-- to be reversible like directory names do.
		stem = "_"
	end

	if #stem > MAX_UNIT_STEM_LEN then
		local finish = MAX_UNIT_STEM_LEN
		-- Don't cut a character in half...
		while finish > 0 and isContinuationByte(string.byte(stem, finish + 1)) do
			finish -= 1
		end
		-- ...or an escape sequence.
		for position = math.max(finish - 1, 1), finish do
			if string.byte(stem, position) == PERCENT then
				finish = position - 1
				break
			end
		end
		stem = string.sub(stem, 1, finish)
	end

	return stem
end)

--[[
	Returns a key that is equal for any two file names that a case-insensitive
	filesystem could consider the same file. Luau can't fold the case of
	non-ASCII letters, so every non-ASCII byte is treated as the same character.
	That can only add an unnecessary suffix, never let one file overwrite
	another.
]]
local collisionKey = memoize(function(fileName: string): string
	return (string.gsub(string.lower(fileName), "[\128-\255]", "*"))
end)

--[[
	Assigns unique file names to sibling entries, in sibling order.
]]
local SiblingNamer = {}
SiblingNamer.__index = SiblingNamer

function SiblingNamer.new()
	return setmetatable({ used = {} }, SiblingNamer)
end

function SiblingNamer:assign(name: string, kind: "Directory" | "Unit"): string
	local stem = if kind == "Directory" then encodeName(name) else unitStem(name)
	local extension = if kind == "Directory" then "" else UNIT_EXTENSION

	local candidate = stem .. extension
	local number = 2
	while self.used[collisionKey(candidate)] do
		candidate = stem .. "~" .. number .. extension
		number += 1
	end
	self.used[collisionKey(candidate)] = true

	return candidate
end

--[[
	Claims a specific file name, for an entry that already has one. Returns
	false if a sibling claimed it first.
]]
function SiblingNamer:reserve(fileName: string): boolean
	local key = collisionKey(fileName)
	if self.used[key] then
		return false
	end

	self.used[key] = true
	return true
end

--[[
	Whether no sibling has claimed a file name yet.
]]
function SiblingNamer:isFree(fileName: string): boolean
	return not self.used[collisionKey(fileName)]
end

--[[
	Splits a file name into its stem and dedup number, where entries without a
	`~N` suffix have the number 1.
]]
local function splitFileName(fileName: string, kind: "Directory" | "Unit"): (string, number)
	local base = fileName
	if kind == "Unit" then
		base = string.sub(fileName, 1, -#UNIT_EXTENSION - 1)
	end

	local stem, suffix = string.match(base, "^(.*)~(%d+)$")
	local number = tonumber(suffix)
	if stem ~= nil and number ~= nil and number >= 2 and tostring(number) == suffix then
		return stem, number
	end

	return base, 1
end

--[[
	Whether `fileName` is a name that SiblingNamer could give to an entry with
	the given name, ignoring which dedup suffix it got.
]]
local function isNameFor(fileName: string, name: string, kind: "Directory" | "Unit"): boolean
	if kind == "Unit" and string.sub(fileName, -#UNIT_EXTENSION) ~= UNIT_EXTENSION then
		return false
	end

	local stem = splitFileName(fileName, kind)
	local expected = if kind == "Directory" then encodeName(name) else unitStem(name)
	return stem == expected
end

--[[
	Returns a key shared by every sibling that SiblingNamer would have to tell
	apart with dedup suffixes.
]]
local function familyKey(name: string, kind: "Directory" | "Unit"): string
	local stem = if kind == "Directory" then encodeName(name) else unitStem(name)
	return kind .. ":" .. collisionKey(stem)
end

--[[
	Returns the familyKey of the entries that SiblingNamer could give a file
	name to, whatever their names' case.
]]
local function fileFamilyKey(fileName: string, kind: "Directory" | "Unit"): string
	return kind .. ":" .. collisionKey((splitFileName(fileName, kind)))
end

--[[
	Orders asset paths so that siblings that only differ by their dedup suffix
	are in suffix order (`Tree`, `Tree~2`, ..., `Tree~10`). This mirrors
	compare_paths in src/assets/names.rs.
]]
local function comparePaths(a: string, b: string): boolean
	local aComponents = string.split(a, "/")
	local bComponents = string.split(b, "/")

	for index = 1, math.min(#aComponents, #bComponents) do
		local aComponent, bComponent = aComponents[index], bComponents[index]
		local aStem, aNumber = splitFileName(aComponent, if index == #aComponents then "Unit" else "Directory")
		local bStem, bNumber = splitFileName(bComponent, if index == #bComponents then "Unit" else "Directory")
		local aKey, bKey = collisionKey(aStem), collisionKey(bStem)

		if aKey ~= bKey then
			return aKey < bKey
		elseif aNumber ~= bNumber then
			return aNumber < bNumber
		elseif aComponent ~= bComponent then
			return aComponent < bComponent
		end
	end

	return #aComponents < #bComponents
end

--[[
	Returns a string that sorts asset paths in the same order as comparePaths
	when compared as strings. Comparing paths with comparePaths over and over
	is slow when sorting thousands of them.
]]
local function pathSortKey(path: string): string
	local components = string.split(path, "/")
	local parts = table.create(#components)

	for index, component in components do
		local stem, number = splitFileName(component, if index == #components then "Unit" else "Directory")
		-- Encoded names never contain control characters, so these separate
		-- the parts unambiguously and sort before anything in a name.
		parts[index] = collisionKey(stem) .. "\0" .. string.format("%010d", number) .. "\0" .. component
	end

	return table.concat(parts, "\1")
end

return {
	UNIT_EXTENSION = UNIT_EXTENSION,
	encodeName = encodeName,
	decodeName = decodeName,
	folderNameFromDir = folderNameFromDir,
	canBeDirectory = canBeDirectory,
	splitFileName = splitFileName,
	isNameFor = isNameFor,
	familyKey = familyKey,
	fileFamilyKey = fileFamilyKey,
	comparePaths = comparePaths,
	pathSortKey = pathSortKey,
	SiblingNamer = SiblingNamer,
}
