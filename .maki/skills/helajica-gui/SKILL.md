---
name: helajica-gui
description: Extend the Helajica desktop app from Lua - sidebar panels, status items by the input, command palette entries, notifications, filling the input box and themes - through maki.gui and the reserved ui.* topics. Use when the user wants to add or change something in the Helajica window without touching Rust.
---

# Extending the Helajica window from Lua

Helajica is maki with a desktop window. Every maki plugin still works; on top
of that a plugin can draw into the window through `maki.gui`. Write the plugin
like any maki plugin (see the Plugins guide: code in
`~/.config/maki/lua/<name>.lua`, loaded from `~/.config/maki/init.lua`, or the
same layout under `<project>/.maki/` for one project). Never write into
maki's own `plugins/` directory.

## The bus

- `maki.gui.active()` - true only inside the desktop app. Return early when
  false so the plugin stays harmless in the terminal.
- `maki.gui.emit(topic, data)` - send to the window. Returns `true`, or
  `nil, err`.
- `maki.gui.recv(topic)` - wait for the next message from the window on
  `topic`. It suspends, so call it inside `maki.async.spawn(function() ... end)`
  and loop. Messages queue until read.

## Reserved topics

Every drawable thing has an `id`. Emitting again with the same `id` replaces
it (that is how you redraw); `{ id = "...", remove = true }` removes it.

| topic | data | effect |
|---|---|---|
| `ui.panel` | `{ id, title, body = { node... } }` | a panel in the sidebar |
| `ui.status` | `{ id, text }` | a short item next to the model, under the input |
| `ui.command` | `{ id, title, description? }` | a command palette entry (Ctrl+K); choosing it sends `ui.command.<id>` |
| `ui.notify` | `{ message, level? }` | a toast; level is `info`, `success`, `warning` or `error` |
| `ui.prompt` | `{ text, send? }` | fills the input; `send = true` also sends it |
| `ui.theme` | `{ name }`, `{ dark }` or `{ config }` | picks a theme from the themes dir, flips dark mode, or applies a theme document inline |

Any other topic is yours: namespace it with the plugin name
(`"git_panel/refresh"`).

## Panel nodes

`body` is a list of nodes, each a table with a `type`:

- `{ type = "text", text, muted? }`
- `{ type = "markdown", text }`
- `{ type = "code", text }`
- `{ type = "badge", text }`
- `{ type = "separator" }`
- `{ type = "progress", value }` - 0 to 100
- `{ type = "button", label, topic, data?, variant? }` - a click sends `data`
  to `topic`; variant is `outline` (default), `primary`, `ghost` or `danger`
- `{ type = "row", children }`, `{ type = "col", children }` - layout

## Pattern

```lua
if not maki.gui.active() then return end

local count = 0
local function draw()
  maki.gui.emit("ui.panel", {
    id = "counter", title = "Counter",
    body = {
      { type = "text", text = "Clicked " .. count .. " times" },
      { type = "button", label = "Click", topic = "counter/click" },
    },
  })
end

maki.async.spawn(function()
  while true do
    maki.gui.recv("counter/click")
    count = count + 1
    draw()
  end
end)

draw()
```

A complete example is `examples/gui/quick_prompts.lua` in the Helajica repo.

## Themes

Theme files are gpui-component theme JSON documents in the GUI config dir's
`themes/` folder (`<maki config dir>/gui/themes/`). The window watches the
folder and reloads on change; pick one with `ui.theme { name = "..." }`.

## After writing

The user restarts the session (New session in the sidebar) to load the plugin.
Panels and status items reset with each session, so draw them at load.
