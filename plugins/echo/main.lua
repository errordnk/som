-- The stage 0 test plugin (PLUGINS_DESIGN.md section 12): shows where it runs,
-- echoes what is typed, and exercises the blocks, processes, timers and som.*.
--
-- Commands typed into its input line:
--   /uname   runs `uname -a` on the plugin's host (Unix hosts)
--   /copy x  puts x into the client's clipboard (som.clipboard_write)
--   /tick    three timer ticks, a second apart
--   /clear   clears the view
-- Anything else is echoed back as markdown.

local count = 0

local function say(text, style)
  return ui.append{ kind = "text", props = { text = text, style = style } }
end

plugin.on("start", function(ctx)
  ui.header{ title = "Echo", subtitle = (host.name() or "?") .. " · " .. host.os() .. " · " .. ctx.cwd, status = "idle" }
  ui.input{ enabled = true, placeholder = "Type something, or /uname, /copy text, /tick, /clear" }
  say("Running on " .. (host.name() or "?") .. " (" .. host.os() .. ") in " .. ctx.cwd, "muted")
end)

plugin.on("submit", function(text)
  count = count + 1
  if text == "/clear" then
    ui.clear()
  elseif text == "/uname" then
    ui.header{ status = "busy" }
    local group = ui.append{ kind = "group", props = { title = "uname -a" } }
    output = { block = ui.append{ parent = group, kind = "code", props = { text = "" } } }
    local ok, err = pcall(proc.spawn, { program = "uname", args = { "-a" } })
    if not ok then
      -- No `uname` there (Windows): the first line of the error, and back to idle.
      output = nil
      ui.header{ status = "idle" }
      say((tostring(err):match("[^\n]*")), "error")
    end
  elseif text:sub(1, 6) == "/copy " then
    local _, err = som.clipboard_write(text:sub(7))
    say(err and ("copy failed: " .. err) or "copied to the clipboard", err and "error" or "muted")
  elseif text == "/tick" then
    timer.every(1000, "tick")
  else
    ui.batch(function()
      ui.append{ kind = "markdown", props = { text = "**#" .. count .. "** you said: " .. text } }
      ui.append{ kind = "kv", props = { rows = { { "length", tostring(#text) }, { "at", tostring(math.floor(time.now())) } } } }
    end)
  end
end)

-- The code block `/uname` writes into, and the lines it has so far.
output = nil
plugin.on("line", function(p, line)
  if not output then return say(line) end
  output[#output + 1] = line
  ui.update(output.block, { text = table.concat(output, "\n") })
end)

plugin.on("exit", function(p, code)
  output = nil
  ui.header{ status = "idle" }
  say("exit " .. tostring(code), "muted")
end)

local ticks = 0
plugin.on("timer", function(name)
  ticks = ticks + 1
  say("tick " .. ticks)
  if ticks % 3 == 0 then timer.cancel(name) end
end)

plugin.on("interrupt", function()
  say("interrupted (Esc)", "muted")
end)

plugin.on("stop", function() log.info("echo: stopped") end)
plugin.on("detach", function() log.info("echo: Som went away") end)
plugin.on("attach", function() log.info("echo: Som is back") end)
