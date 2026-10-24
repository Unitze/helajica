local M = {}

M.name = "maki-subagents"
M.description =
  "Split work across subagents with the task tool: when to delegate, which type and model tier to pick, running them in parallel or in the background, and writing prompts that return useful results. Load before delegating anything bigger than one lookup, or when the user asks for subagents or parallel work."

M.content = [[
# Working with subagents

The `task` tool starts a subagent: a fresh agent with its own context that
does one job and returns one result to you. The user never sees that
result, only what you say about it.

## When to delegate

- Exploration that would fill your context with file dumps: searching
  many files, reading a large module, tracing a call path. You keep the
  conclusion, not the reads.
- Independent pieces of one change that touch different files.
- A second opinion that should not start from your conclusion.

Do it yourself when it is a few tool calls, when you already know the
file and line, or when the pieces depend on each other's results.

## Picking the subagent

- `subagent_type`: `research` (default) has read-only tools; use it for
  finding and explaining. `general` can edit files and run commands; use
  it only for work you would otherwise do yourself.
- `model_tier`: `weak` for search and summaries, `medium` for ordinary
  edits, `strong` only for subtle bugs and design. Omit it to use your own
  model. A cheaper tier for a lookup is the main saving subagents give.
- `output_schema`: set it when you will process the result, so it comes
  back as validated JSON instead of prose.

## Writing the prompt

Each subagent starts with nothing, so the prompt is all it knows:

1. The goal, in one or two sentences.
2. What you already know or ruled out, with paths and line numbers.
3. Where to look, and what is out of scope.
4. What to return: a short summary with `file:line` references, never
   whole files. Ask for a fixed shape (list, table, JSON) when you will
   compare several results.

Do not hand it your hypothesis to confirm; ask it to find what is true.

## Running several at once

Put several `task` calls in one `batch` call, or in one turn, and they run
together, up to the configured limit. Split by area (one per directory or
question), not by step, so no two subagents edit the same file.

## Background subagents

When the `task` tool has a `background` input, a background subagent runs
while you keep working, and its result arrives later as a notice. Use it
for long jobs whose result you do not need for your next step. End your
turn when you have nothing else to do; the notice wakes you. Background
subagents cannot ask the user for permission, so give them work that does
not need any. Without the `background` input, the user can switch it on
in Settings › Add-ons › Background subagents.

## After they finish

Check what a subagent reports before you build on it: open the files it
names when the claim matters. Then tell the user what was found or
changed, in your own words.
]]

return M
