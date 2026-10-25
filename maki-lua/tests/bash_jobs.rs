use std::sync::Arc;

use maki_agent::AgentMode;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_lua::PluginHost;

const BASH: &str = include_str!("../../plugins/bash/init.lua");
const PRELUDE: &str = r#"
local tools, timers, notices, jobs, stops, cancel_hooks = {}, {}, {}, {}, {}, {}
local register_tool = maki.api.register_tool
local next_job = 0
maki.api.register_tool = function(tool) tools[tool.name] = tool end
maki.gui.active = function() return false end
maki.defer_fn = function(fn, ms) timers[#timers + 1] = { fn = fn, ms = ms } end
maki.async.on_cancel = function(fn) cancel_hooks[#cancel_hooks + 1] = fn end
maki.fn.jobstart = function(command, opts)
  next_job = next_job + 1
  jobs[next_job] = opts
  return next_job
end
maki.fn.jobstop = function(id) stops[id] = true return true end
maki.session.notify = function(text, opts)
  notices[#notices + 1] = { text = text, opts = opts }
  return true
end
local function context(session)
  return {
    results = {},
    session_id = function() return session end,
    background_session_id = function() return session end,
    output_mode = function() return 'full' end,
    tool_output_lines = function() return {} end,
    set_deadline = function() end,
    finish = function(self, result) self.results[#self.results + 1] = result end,
  }
end
local chat = '01965087-4c71-7f00-8000-000000000000'
"#;

const CHECKS: &str = r#"
local ctx = context(chat)
local started = tools.bash.handler({ command = 'cargo test', background = true }, ctx)
assert(started.llm_output:find('background', 1, true), 'explicit background must return immediately')
assert(jobs[1].scope.session == chat, 'background job must outlive the tool task')
jobs[1].on_stdout(1, 'test output')
jobs[1].on_exit(1, 1)
assert(#notices == 1 and notices[1].text:find('code 1', 1, true), 'nonzero exit must be reported')
assert(notices[1].opts.session == chat and notices[1].opts.wake, 'completion must wake its owning chat')

ctx = context(chat)
tools.bash.handler({ command = 'cargo check' }, ctx)
local slow = next_job
assert(jobs[slow].scope.session == chat, 'automatic background needs session ownership from the start')
jobs[slow].on_stdout(slow, 'building')
assert(#ctx.results == 0 and #timers == 1, 'short commands must initially wait')
assert(timers[1].ms == 10000, 'automatic yield must happen after ten seconds')
timers[1].fn()
assert(#ctx.results == 1 and ctx.results[1].llm_output:find('building', 1, true), 'yield preserves partial output')
cancel_hooks[#cancel_hooks]('cancelled')
assert(not stops[slow], 'a finished tool must not kill its background job')
jobs[slow].on_stdout(slow, 'finished')
jobs[slow].on_exit(slow, 0)
assert(#notices == 2 and notices[2].text:find('finished', 1, true), 'detached jobs still deliver their final output')
assert(#ctx.results == 1, 'exit must not finish the tool twice')

ctx = context(chat)
tools.bash.handler({ command = 'git status' }, ctx)
local quick = next_job
jobs[quick].on_stdout(quick, 'clean')
jobs[quick].on_exit(quick, 0)
timers[#timers].fn()
assert(#ctx.results == 1 and ctx.results[1].llm_output == 'clean', 'fast commands must return normally')
assert(#notices == 2, 'fast commands must not also send a background notice')

ctx = context(chat)
local timer_count = #timers
tools.bash.handler({ command = 'cargo check', background = false }, ctx)
assert(jobs[next_job].scope == nil and #timers == timer_count, 'explicit synchronous mode must remain task scoped')
jobs[next_job].on_exit(next_job, 0)
assert(#ctx.results == 1, 'synchronous mode must finish normally')

ctx = context(nil)
tools.bash.handler({ command = 'cargo check', background = true }, ctx)
assert(jobs[next_job].scope == nil and #timers == timer_count, 'headless calls without a chat must stay synchronous')
jobs[next_job].on_exit(next_job, 0)

ctx = context(chat)
tools.bash.handler({ command = 'cargo check' }, ctx)
local cancelled = next_job
cancel_hooks[#cancel_hooks]('cancelled')
timers[#timers].fn()
assert(stops[cancelled], 'cancel before automatic yield must stop the foreground job')
assert(#ctx.results == 1, 'cancel must settle once')

ctx = context(chat)
tools.bash.handler({ command = 'server', background = true }, ctx)
local stopped = next_job
tools.bash_stop.handler({ job = stopped })
jobs[stopped].on_exit(stopped, 1)
assert(stops[stopped] and #notices == 2, 'explicit stop must not send a duplicate completion notice')
"#;

#[test]
fn shell_jobs_yield_and_report_without_losing_ownership_or_finishing_twice() {
    let registry = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let source = format!(
        "{PRELUDE}\n{BASH}\nregister_tool({{ name = 'check_bash_jobs', description = 'Verify shell job lifecycle', schema = {{ type = 'object', properties = {{}} }}, handler = function()\n{CHECKS}\nreturn 'ok' end }})"
    );
    host.load_source("bash", &source).unwrap();
    let tool = registry.get("check_bash_jobs").unwrap();
    let invocation = tool.tool.parse(&serde_json::json!({})).unwrap();
    let result = smol::block_on(invocation.execute(&stub_ctx(&AgentMode::Build)));
    assert!(result.output.is_ok(), "{:?}", result.output);
}
