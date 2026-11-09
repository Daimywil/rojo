local Rojo = script:FindFirstAncestor("Rojo")
local Plugin = Rojo.Plugin
local Packages = Rojo.Packages

local Roact = require(Packages.Roact)

local Config = require(Plugin.Config)

local Theme = require(Plugin.App.Theme)
local BorderedContainer = require(Plugin.App.Components.BorderedContainer)
local ScrollingFrame = require(Plugin.App.Components.ScrollingFrame)
local TextButton = require(Plugin.App.Components.TextButton)
local Header = require(Plugin.App.Components.Header)
local Tooltip = require(Plugin.App.Components.Tooltip)

local PORT_WIDTH = 74
local DIVIDER_WIDTH = 1
local HOST_OFFSET = 12

local SERVER_HEIGHT = 44
local SERVER_PADDING = 6
-- How many servers are shown before the list scrolls.
local MAX_VISIBLE_SERVERS = 3

local e = Roact.createElement

local function ServerEntry(props)
	local server = props.server

	local detail = string.format("%s:%s", server.host, server.port)
	if server.unavailable ~= nil then
		detail ..= " · " .. server.unavailable
	elseif server.place == "this" then
		detail ..= " · this place"
	end

	return Theme.with(function(theme)
		return e(BorderedContainer, {
			transparency = props.transparency,
			size = UDim2.new(1, -10, 0, SERVER_HEIGHT),
			layoutOrder = props.layoutOrder,
		}, {
			Name = e("TextLabel", {
				Text = server.projectName,
				FontFace = theme.Font.Main,
				TextSize = theme.TextSize.Body,
				TextColor3 = if server.unavailable ~= nil then theme.SubTextColor else theme.TextColor,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextTruncate = Enum.TextTruncate.AtEnd,
				TextTransparency = props.transparency,

				Size = UDim2.new(1, -110, 0, 18),
				Position = UDim2.new(0, 12, 0, 5),
				BackgroundTransparency = 1,
			}),

			Detail = e("TextLabel", {
				Text = detail,
				FontFace = theme.Font.Code,
				TextSize = theme.TextSize.Small,
				TextColor3 = theme.SubTextColor,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextTruncate = Enum.TextTruncate.AtEnd,
				TextTransparency = props.transparency,

				Size = UDim2.new(1, -110, 0, 14),
				Position = UDim2.new(0, 12, 0, 24),
				BackgroundTransparency = 1,
			}),

			Connect = if server.unavailable == nil
				then e(TextButton, {
					text = "Connect",
					style = "Solid",
					transparency = props.transparency,
					position = UDim2.new(1, -4, 0.5, 0),
					anchorPoint = Vector2.new(1, 0.5),
					onClick = function()
						props.onConnect(server)
					end,
				}, {
					Tip = e(Tooltip.Trigger, {
						text = string.format("Connect to %s on port %s", server.projectName, server.port),
					}),
				})
				else nil,
		})
	end)
end

--[[
	The Rojo servers running on this machine, each with a button to connect to
	it. Nothing is shown until the first search for them finished.
]]
local function ServerList(props)
	local servers = props.servers
	if servers == nil then
		return nil
	end

	return Theme.with(function(theme)
		if #servers == 0 then
			return e("TextLabel", {
				Text = "No running Rojo servers found. Enter the port of one below.",
				FontFace = theme.Font.Main,
				TextSize = theme.TextSize.Small,
				TextColor3 = theme.SubTextColor,
				TextTransparency = props.transparency,
				TextWrapped = true,

				Size = UDim2.new(1, 0, 0, 32),
				LayoutOrder = props.layoutOrder,
				BackgroundTransparency = 1,
			})
		end

		local entries = {
			Layout = e("UIListLayout", {
				FillDirection = Enum.FillDirection.Vertical,
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, SERVER_PADDING),
			}),
		}
		for index, server in servers do
			entries[server.host .. ":" .. server.port] = e(ServerEntry, {
				server = server,
				onConnect = props.onConnect,
				transparency = props.transparency,
				layoutOrder = index,
			})
		end

		local visible = math.min(#servers, MAX_VISIBLE_SERVERS)

		return e("Frame", {
			Size = UDim2.new(1, 0, 0, visible * SERVER_HEIGHT + (visible - 1) * SERVER_PADDING),
			LayoutOrder = props.layoutOrder,
			BackgroundTransparency = 1,
		}, {
			List = e(ScrollingFrame, {
				size = UDim2.new(1, 0, 1, 0),
				scrollingDirection = Enum.ScrollingDirection.Y,
				transparency = props.transparency,
			}, entries),
		})
	end)
end

local function AddressEntry(props)
	return Theme.with(function(theme)
		return e(BorderedContainer, {
			transparency = props.transparency,
			size = UDim2.new(1, 0, 0, 36),
			layoutOrder = props.layoutOrder,
		}, {
			Host = e("TextBox", {
				Text = props.host or "",
				FontFace = theme.Font.Code,
				TextSize = theme.TextSize.Large,
				TextColor3 = theme.AddressEntry.TextColor,
				TextXAlignment = Enum.TextXAlignment.Left,
				TextTransparency = props.transparency,
				PlaceholderText = Config.defaultHost,
				PlaceholderColor3 = theme.AddressEntry.PlaceholderColor,
				ClearTextOnFocus = false,

				Size = UDim2.new(1, -(HOST_OFFSET + DIVIDER_WIDTH + PORT_WIDTH), 1, 0),
				Position = UDim2.new(0, HOST_OFFSET, 0, 0),

				ClipsDescendants = true,
				BackgroundTransparency = 1,

				[Roact.Change.Text] = function(object)
					if props.onHostChange ~= nil then
						props.onHostChange(object.Text)
					end
				end,
			}),

			Port = e("TextBox", {
				Text = props.port or "",
				FontFace = theme.Font.Code,
				TextSize = theme.TextSize.Large,
				TextColor3 = theme.AddressEntry.TextColor,
				TextTransparency = props.transparency,
				PlaceholderText = Config.defaultPort,
				PlaceholderColor3 = theme.AddressEntry.PlaceholderColor,
				ClearTextOnFocus = false,

				Size = UDim2.new(0, PORT_WIDTH, 1, 0),
				Position = UDim2.new(1, 0, 0, 0),
				AnchorPoint = Vector2.new(1, 0),

				ClipsDescendants = true,
				BackgroundTransparency = 1,

				[Roact.Change.Text] = function(object)
					local text = object.Text
					text = text:gsub("%D", "")
					object.Text = text

					if props.onPortChange ~= nil then
						props.onPortChange(text)
					end
				end,
			}, {
				Divider = e("Frame", {
					BackgroundColor3 = theme.BorderedContainer.BorderColor,
					BackgroundTransparency = props.transparency,
					Size = UDim2.new(0, DIVIDER_WIDTH, 1, 0),
					AnchorPoint = Vector2.new(1, 0),
					BorderSizePixel = 0,
				}),
			}),
		})
	end)
end

local NotConnectedPage = Roact.Component:extend("NotConnectedPage")

function NotConnectedPage:render()
	return Roact.createFragment({
		Header = e(Header, {
			transparency = self.props.transparency,
			layoutOrder = 1,
		}),

		Servers = e(ServerList, {
			servers = self.props.servers,
			onConnect = self.props.onConnectToServer,
			transparency = self.props.transparency,
			layoutOrder = 2,
		}),

		AddressEntry = e(AddressEntry, {
			host = self.props.host,
			port = self.props.port,
			onHostChange = self.props.onHostChange,
			onPortChange = self.props.onPortChange,
			transparency = self.props.transparency,
			layoutOrder = 3,
		}),

		Buttons = e("Frame", {
			Size = UDim2.new(1, 0, 0, 34),
			LayoutOrder = 4,
			BackgroundTransparency = 1,
			ZIndex = 2,
		}, {
			Settings = e(TextButton, {
				text = "Settings",
				style = "Bordered",
				transparency = self.props.transparency,
				layoutOrder = 1,
				onClick = self.props.onNavigateSettings,
			}, {
				Tip = e(Tooltip.Trigger, {
					text = "View and modify plugin settings",
				}),
			}),

			Connect = e(TextButton, {
				text = "Connect",
				style = "Solid",
				transparency = self.props.transparency,
				layoutOrder = 2,
				onClick = self.props.onConnect,
			}, {
				Tip = e(Tooltip.Trigger, {
					text = "Connect to a Rojo sync server",
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
			HorizontalAlignment = Enum.HorizontalAlignment.Center,
			VerticalAlignment = Enum.VerticalAlignment.Center,
			FillDirection = Enum.FillDirection.Vertical,
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 10),
		}),

		Padding = e("UIPadding", {
			PaddingLeft = UDim.new(0, 20),
			PaddingRight = UDim.new(0, 20),
		}),
	})
end

return NotConnectedPage
