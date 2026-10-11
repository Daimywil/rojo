--[[
	Asks the user what to do with assets that changed both in Studio and on
	disk since they were last synced, or to confirm the first sync of a place.
]]

local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin
local Packages = Rojo.Packages

local Roact = require(Packages.Roact)

local Theme = require(Plugin.App.Theme)
local getTextBoundsAsync = require(Plugin.App.getTextBoundsAsync)

local TextButton = require(Plugin.App.Components.TextButton)
local Header = require(Plugin.App.Components.Header)
local BorderedContainer = require(Plugin.App.Components.BorderedContainer)
local ScrollingFrame = require(Plugin.App.Components.ScrollingFrame)
local Tooltip = require(Plugin.App.Components.Tooltip)

local e = Roact.createElement

local PADDING = Vector2.new(13, 10)
local MAX_LISTED = 500

local function plural(count: number, singular: string, pluralForm: string?): string
	return string.format("%d %s", count, if count == 1 then singular else pluralForm or singular .. "s")
end

-- Why Studio and the files disagree, in the order they're listed.
local REASONS = {
	{ id = "bothChanged", text = "Changed both in Studio and on disk" },
	{
		id = "unsavedPlace",
		text = "Changed in Studio, but the place wasn't saved after the last sync, so Studio's version may be older",
	},
	{
		id = "newInUnsavedPlace",
		text = "Only in Studio, but the place wasn't saved after the last sync, so it may have been deleted since",
	},
	{ id = "deletedOnDisk", text = "Changed in Studio, but deleted on disk" },
	{
		id = "missingInStudio",
		text = "On disk, but missing from Studio (deleted in Studio, or lost because the place wasn't saved)",
	},
}

local function describe(summary): string
	local lines = {}

	if #summary.conflicts > 0 then
		table.insert(
			lines,
			string.format(
				"It's unclear whether Studio or the files are right for %s in %s since they were last synced.",
				plural(#summary.conflicts, "asset"),
				summary.path
			)
		)

		local knownReasons = {}
		for _, reason in REASONS do
			knownReasons[reason.id] = true
		end

		local listed = 0
		for _, reason in REASONS do
			local paths = {}
			for _, conflict in summary.conflicts do
				-- Reasons this version doesn't know about are listed as if both
				-- sides changed.
				local id = if knownReasons[conflict.reason] then conflict.reason else "bothChanged"
				if id == reason.id then
					table.insert(paths, conflict.path)
				end
			end

			if #paths == 0 then
				continue
			end

			table.insert(lines, "")
			table.insert(lines, reason.text .. ":")
			for _, path in paths do
				listed += 1
				if listed > MAX_LISTED then
					break
				end
				table.insert(lines, "  " .. path)
			end
		end

		if listed > MAX_LISTED then
			table.insert(lines, string.format("...and %d more", #summary.conflicts - MAX_LISTED))
		end

		table.insert(lines, "")
		table.insert(lines, "Pick which side to keep for all of them. The other side is overwritten.")
	else
		table.insert(
			lines,
			string.format(
				"This place has never synced the assets in %s before. Syncing merges what's in Studio with what's on disk.",
				summary.path
			)
		)
	end

	local other = {}
	if summary.toFs > 0 then
		table.insert(other, plural(summary.toFs, "asset") .. " will be written to files")
	end
	if summary.deleteFs > 0 then
		table.insert(other, plural(summary.deleteFs, "file") .. " will be deleted")
	end
	if summary.toStudio > 0 then
		table.insert(other, plural(summary.toStudio, "asset") .. " will be updated in Studio")
	end
	if summary.deleteStudio > 0 then
		table.insert(other, plural(summary.deleteStudio, "asset") .. " will be deleted from Studio")
	end

	if #other > 0 then
		table.insert(lines, "")
		table.insert(lines, (if #summary.conflicts > 0 then "Also, " else "") .. table.concat(other, ", ") .. ".")
	end

	return table.concat(lines, "\n")
end

local Summary = Roact.Component:extend("AssetConflictsSummary")

function Summary:init()
	self.contentSize, self.setContentSize = Roact.createBinding(Vector2.new(0, 0))
end

function Summary:render()
	local text = describe(self.props.summary)

	return Theme.with(function(theme)
		return e(BorderedContainer, {
			size = Roact.joinBindings({
				containerSize = self.props.containerSize,
				contentSize = self.contentSize,
			}):map(function(values)
				local maximumSize = values.containerSize
				maximumSize -= Vector2.new(14, 14) * 2 -- Page padding
				maximumSize -= Vector2.new(0, 34 + 10) -- Buttons and spacing
				maximumSize -= Vector2.new(0, 32 + 10) -- Header and spacing

				local outerSize = values.contentSize + PADDING * 2

				return UDim2.new(1, 0, 0, math.min(outerSize.Y, maximumSize.Y))
			end),
			transparency = self.props.transparency,
			layoutOrder = self.props.layoutOrder,
		}, {
			ScrollingFrame = e(ScrollingFrame, {
				size = UDim2.new(1, 0, 1, 0),
				contentSize = self.contentSize:map(function(value)
					return value + PADDING * 2
				end),
				transparency = self.props.transparency,

				[Roact.Change.AbsoluteSize] = function(object)
					local containerSize = object.AbsoluteSize - PADDING * 2
					local textBounds = getTextBoundsAsync(text, theme.Font.Code, theme.TextSize.Code, containerSize.X)

					self.setContentSize(Vector2.new(containerSize.X, textBounds.Y))
				end,
			}, {
				Text = e("TextBox", {
					Text = text,
					TextEditable = false,
					ClearTextOnFocus = false,
					FontFace = theme.Font.Code,
					TextSize = theme.TextSize.Code,
					TextColor3 = theme.TextColor,
					TextXAlignment = Enum.TextXAlignment.Left,
					TextYAlignment = Enum.TextYAlignment.Top,
					TextTransparency = self.props.transparency,
					TextWrapped = true,
					BackgroundTransparency = 1,
					Size = UDim2.new(1, 0, 1, 0),
				}),

				Padding = e("UIPadding", {
					PaddingLeft = UDim.new(0, PADDING.X),
					PaddingRight = UDim.new(0, PADDING.X),
					PaddingTop = UDim.new(0, PADDING.Y),
					PaddingBottom = UDim.new(0, PADDING.Y),
				}),
			}),
		})
	end)
end

local AssetConflictsPage = Roact.Component:extend("AssetConflictsPage")

function AssetConflictsPage:init()
	self.containerSize, self.setContainerSize = Roact.createBinding(Vector2.new(0, 0))
end

function AssetConflictsPage:render()
	local summary = self.state.summary
	if summary == nil then
		return nil
	end

	local hasConflicts = #summary.conflicts > 0

	return e("Frame", {
		Size = UDim2.new(1, 0, 1, 0),
		BackgroundTransparency = 1,

		[Roact.Change.AbsoluteSize] = function(object)
			self.setContainerSize(object.AbsoluteSize)
		end,
	}, {
		Header = e(Header, {
			transparency = self.props.transparency,
			layoutOrder = 1,
		}),

		Summary = e(Summary, {
			summary = summary,
			containerSize = self.containerSize,
			transparency = self.props.transparency,
			layoutOrder = 2,
		}),

		Buttons = e("Frame", {
			Size = UDim2.new(1, 0, 0, 34),
			LayoutOrder = 3,
			BackgroundTransparency = 1,
		}, {
			Abort = e(TextButton, {
				text = "Abort",
				style = "Bordered",
				transparency = self.props.transparency,
				layoutOrder = 1,
				onClick = function()
					self.props.onDecision("Abort")
				end,
			}, {
				Tip = e(Tooltip.Trigger, {
					text = "Disconnect without changing anything",
				}),
			}),

			KeepFiles = if hasConflicts
				then e(TextButton, {
					text = "Keep Files",
					style = "Bordered",
					transparency = self.props.transparency,
					layoutOrder = 2,
					onClick = function()
						self.props.onDecision("KeepFiles")
					end,
				}, {
					Tip = e(Tooltip.Trigger, {
						text = "Replace these assets in Studio with the files on disk",
					}),
				})
				else nil,

			KeepStudio = e(TextButton, {
				text = if hasConflicts then "Keep Studio" else "Continue",
				style = "Solid",
				transparency = self.props.transparency,
				layoutOrder = 3,
				onClick = function()
					self.props.onDecision("KeepStudio")
				end,
			}, {
				Tip = e(Tooltip.Trigger, {
					text = if hasConflicts
						then "Overwrite the files on disk with these assets from Studio"
						else "Sync the assets",
				}),
			}),

			Layout = e("UIListLayout", {
				HorizontalAlignment = Enum.HorizontalAlignment.Right,
				FillDirection = Enum.FillDirection.Horizontal,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 10),
			}),
		}),

		Layout = e("UIListLayout", {
			VerticalAlignment = Enum.VerticalAlignment.Center,
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 10),
		}),

		Padding = e("UIPadding", {
			PaddingLeft = UDim.new(0, 14),
			PaddingRight = UDim.new(0, 14),
			PaddingTop = UDim.new(0, 14),
			PaddingBottom = UDim.new(0, 14),
		}),
	})
end

function AssetConflictsPage.getDerivedStateFromProps(props)
	-- Keep the last summary around so the page can still be animated out
	-- after the decision is made.
	if props.summary ~= nil then
		return {
			summary = props.summary,
		}
	end

	return nil
end

return AssetConflictsPage
