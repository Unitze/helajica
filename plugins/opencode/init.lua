local ToolView = require("maki.tool_view")
local output_limits = require("maki.output_limits")

local DEFAULT_OUTPUT_LINES = 5
local MODEL_CONFIG_ID = "model"

local opts = maki.api.register_options({
  cmd = {
    type = "string",
    default = "opencode",
    desc = "Executable that serves ACP (`opencode` with args `acp`).",
  },
  args = {
    type = "string",
    default = "acp",
    desc = "Single command line argument for the executable. `opencode` serves ACP behind `acp`.",
  },
  model = {
    type = "string",
    desc = "Default model spec, resolved by OpenCode's own config (e.g. `opencode/grok-code`). Unset uses OpenCode's default.",
  },
  policy = {
    type = "string",
    default = "allow",
    desc = "How to answer OpenCode's permission requests: 'allow' runs unattended, 'deny' refuses each call, 'cancel' refuses the whole turn.",
  },
  max_concurrent = {
    type = "integer",
    default = 4,
    min = 1,
    desc = "Max concurrently running OpenCode turns.",
  },
})

local description = [[Delegate a task to OpenCode, an external agent with its own tools, models and permissions.

OpenCode works autonomously in the current working directory: it searches, reads, edits and runs commands itself, under its own model and its own permission config. The result is OpenCode's final text; it is not visible to the user, so summarize it in your response.

Notes:
1. Inline everything the task needs into the prompt, each call starts a fresh session.
2. Ask for concise summaries with file:line references, not full file contents.
3. OpenCode answers with the model its own config picks; a specific one can be set in the plugin options.
4. Use for a second opinion from a different model or to offload self-contained work.]]

local examples = {
  {
    description = "Independent review of a change",
    prompt = "Review the working tree changes. Judge the design and the error handling, and list concrete improvements with file paths.",
  },
}

local client
local client_err
local semaphore = maki.async.semaphore(opts.max_concurrent)

local function ensure_client()
  if client or client_err then
    return client, client_err
  end
  local c, err = maki.acp.client({
    cmd = opts.cmd,
    args = { opts.args },
    policy = opts.policy,
  })
  if err then
    client_err = "opencode client: " .. err
    return nil, client_err
  end
  client = c
  return client
end

local function handler(input)
  local c, c_err = ensure_client()
  if not c then
    return { llm_output = c_err, is_error = true }
  end

  local permit = semaphore:acquire()
  local sess

  local ok, out = pcall(function()
    local s, s_err = c:new_session()
    if s_err then
      return { llm_output = "opencode session: " .. s_err, is_error = true }
    end
    sess = s

    if opts.model and opts.model ~= "" then
      local m_ok, m_err = sess:set_config(MODEL_CONFIG_ID, opts.model)
      if not m_ok then
        return { llm_output = "opencode model not set: " .. m_err, is_error = true }
      end
    end

    maki.async.on_cancel(function()
      sess:cancel()
    end)

    local result, prompt_err = sess:prompt(input.prompt)
    if prompt_err then
      return { llm_output = "opencode: " .. prompt_err, is_error = true }
    end
    return { llm_output = result.text }
  end)

  permit:release()
  if not ok then
    error(out, 0)
  end
  return out
end

local function header(input)
  return input.description
end

local function restore(_input, output, is_error, ctx)
  local tol = ctx:tool_output_lines()
  return ToolView.restore_markdown(output, is_error, {
    max_lines = (tol and tol.task) or DEFAULT_OUTPUT_LINES,
    keep = "head",
    max_line_bytes = output_limits.DEFAULT_MAX_LINE_BYTES,
  })
end

maki.api.register_tool({
  name = "opencode",
  description = description,
  kind = "execute",
  audiences = { "main", "workflow" },
  examples = examples,
  permission = "run",
  permission_scopes = "description",
  schema = {
    type = "object",
    required = { "description", "prompt" },
    additionalProperties = false,
    properties = {
      description = {
        type = "string",
        description = "Short (3-5 words) description of the task",
      },
      prompt = {
        type = "string",
        description = "Detailed task prompt for OpenCode",
      },
    },
  },
  handler = handler,
  header = header,
  restore = restore,
})
