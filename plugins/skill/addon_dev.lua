local M = {}

M.name = "helajica-addon"
M.description =
  "Write or fix a Helajica add-on: a Lua package that adds tools, rewrites the system prompt, wraps or replaces the agent's tools, or adds commands and GUI panels. Load whenever the user asks for an add-on, extension or change to how the agent itself works."
M.addons_dir_placeholder = "__HELAJICA_ADDONS_DIR__"

M.content = [[
# Writing a Helajica add-on

An add-on is a Lua package the user wrote, loaded at the start of every
session. It runs with the same `maki` API as any plugin, so it can change
the engine itself: the tools the agent has, its system prompt, and how
existing tools behave. For the full API, load the `maki-plugin-dev` skill.

## Where it goes

```
__HELAJICA_ADDONS_DIR__/<id>/
  plugin.toml        permissions the add-on uses
  plugin/init.lua    entry file; every plugin/*.lua runs, sorted
  lua/<module>.lua   optional modules, for require("<module>")
```

`<id>` is short kebab-case and must not match a bundled plugin name. A
package lists every permission it uses; one it does not name is denied:

```toml
[permissions]
fs_read = true    # read files and maki's own directories
fs_write = false  # write files
net = false       # HTTP; add net_hosts = ["api.example.com"] to narrow it
run = false       # run processes
env = false       # read environment variables
agent = false     # drive the agent, call its tools, run subagents
```

Grant only what the add-on needs.

## What an add-on can change

Add a tool:

```lua
maki.api.register_tool({
  name = "word_count",
  description = "Count the words in a piece of text.",
  schema = {
    type = "object",
    properties = { text = { type = "string", description = "Text to count", required = true } },
  },
  handler = function(input)
    local n = 0
    for _ in input.text:gmatch("%S+") do
      n = n + 1
    end
    return tostring(n)
  end,
})
```

Rewrite the system prompt, whole or one section (`identity`, `tone`,
`tool_usage`, `efficient_tools`, `conventions`, `after_instructions`,
`objectivity`, `completion`):

```lua
maki.api.set_prompt({ slot = "tone", content = "Answer in Japanese, briefly." })
```

Add to a section without owning it:

```lua
maki.api.register_prompt_hint({ slot = "conventions", content = "- Run the tests before saying done." })
```

Wrap an existing tool, to change its input, output or when it runs, with
`maki.api.set_slot`, or hide one with `maki.setup({ agent = { disabled_tools = { "webfetch" } } })`.
Slash commands come from `maki.api.register_command`, events from
`maki.api.create_autocmd`, and a sidebar panel or status item in the
desktop app from `maki.gui`. Look each one up in `maki-plugin-dev` before
using it: names and fields above are a starting point, not the whole API.

## Apply and check

1. Write the files, then check `plugin.toml` and `plugin/init.lua` exist.
2. The add-on loads with the next session. In the desktop app it appears
   under Settings › Add-ons with a switch; switching reopens the session.
3. Start a new session, confirm the change took (the tool is listed, the
   prompt behaves), and look in maki's log for load errors if it did not.
4. Tell the user the add-on's id, its folder, what it changes, and the
   permissions it holds.

A broken add-on never stops the app: switch it off in Settings › Add-ons
or delete its folder.
]]

return M
