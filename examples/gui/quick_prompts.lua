-- Quick prompts: a sidebar panel of one-click prompts, a palette command
-- that runs a review, and a status item that counts what you sent.
--
-- Install: copy to ~/.config/maki/lua/quick_prompts.lua and add
-- `require("quick_prompts")` to ~/.config/maki/init.lua.

if not maki.gui.active() then
  return
end

local PROMPTS = {
  { id = "explain", label = "Explain this project", text = "Give me a short tour of this project." },
  { id = "tests", label = "Run the tests", text = "Run the test suite and summarize failures." },
  { id = "review", label = "Review my diff", text = "Review my uncommitted changes for bugs." },
}

local sent = 0

local function draw()
  local buttons = {}
  for _, p in ipairs(PROMPTS) do
    table.insert(buttons, {
      type = "button",
      label = p.label,
      topic = "quick_prompts/send",
      data = { id = p.id },
      variant = "ghost",
    })
  end
  maki.gui.emit("ui.panel", { id = "quick_prompts", title = "Quick prompts", body = buttons })
  maki.gui.emit("ui.status", { id = "quick_prompts", text = sent .. " quick" })
end

local function find(id)
  for _, p in ipairs(PROMPTS) do
    if p.id == id then
      return p
    end
  end
end

local function send(text)
  sent = sent + 1
  maki.gui.emit("ui.prompt", { text = text, send = true })
  draw()
end

maki.gui.emit("ui.command", {
  id = "quick-review",
  title = "Quick prompts: review my diff",
  description = "Send the review prompt",
})

-- Panel buttons answer on the topic they were given.
maki.async.spawn(function()
  while true do
    local msg = maki.gui.recv("quick_prompts/send")
    local prompt = msg and find(msg.id)
    if prompt then
      send(prompt.text)
    end
  end
end)

-- Palette commands answer on `ui.command.<id>`.
maki.async.spawn(function()
  while true do
    maki.gui.recv("ui.command.quick-review")
    send(find("review").text)
  end
end)

draw()
