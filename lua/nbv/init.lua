-- nbv: the Lua companion loaded into the embedded Neovim (R6).
--
-- It provides commands, keymaps, cell motions, extmark placement, write/read interception
-- and RPC notifications. It holds no notebook state: cell identity, the document, and the
-- kernel all live in Rust. Rust loads this file before the user's config (pre-config) and
-- it finishes its setup on VimEnter (post-config).

local M = {}
local api = vim.api

--- Channel of the nbv process, set at load.
M.chan = nil
--- Path of the projected notebook buffer (the argument file), set at load.
M.path = nil
M.ns = api.nvim_create_namespace('nbv')
M.slots = 64

local function notify(action, extra)
  local args = extra or {}
  args.line = api.nvim_win_get_cursor(0)[1] - 1
  args.tick = api.nvim_buf_get_changedtick(0)
  vim.rpcnotify(M.chan, 'nbv', action, args)
end

--- The marker grammar (§7.1), for motions. Rust's parser is authoritative.
function M.is_marker(line)
  if line:sub(1, 4) ~= '# %%' then
    return false
  end
  local rest = line:sub(5)
  for _, kind in ipairs({ ' [markdown]', ' [raw]' }) do
    if rest:sub(1, #kind) == kind then
      rest = rest:sub(#kind + 1)
      break
    end
  end
  if rest == '' then
    return true
  end
  if rest:sub(1, 4) ~= ' id=' then
    return false
  end
  local lit = rest:sub(5)
  if #lit < 2 or lit:sub(1, 1) ~= '"' or lit:sub(-1) ~= '"' then
    return false
  end
  local ok, v = pcall(vim.json.decode, lit)
  return ok and type(v) == 'string'
end

--- Marker line numbers (1-based) of the current buffer.
local function markers(buf)
  local out = {}
  for i, l in ipairs(api.nvim_buf_get_lines(buf, 0, -1, false)) do
    if M.is_marker(l) then
      out[#out + 1] = i
    end
  end
  return out
end

--- Moves to the first body line of the `count`th next (dir=1) or previous (dir=-1) cell.
function M.jump(dir)
  local row = api.nvim_win_get_cursor(0)[1]
  local ms = markers(0)
  -- The marker of the cell containing the cursor.
  local cur = 0
  for i, m in ipairs(ms) do
    if m <= row then
      cur = i
    end
  end
  local target = cur + dir * vim.v.count1
  if dir < 0 and ms[cur] and row > ms[cur] + 1 then
    target = target + 1 -- first go to the top of the current cell
  end
  target = math.max(1, math.min(#ms, target))
  if ms[target] then
    local last = api.nvim_buf_line_count(0)
    vim.cmd("normal! m'")
    api.nvim_win_set_cursor(0, { math.min(ms[target] + 1, last), 0 })
  end
end

--- Selects the current cell linewise: body only (`ic`) or marker and body (`ac`).
function M.select(around)
  local row = api.nvim_win_get_cursor(0)[1]
  local ms = markers(0)
  local start, stop = nil, api.nvim_buf_line_count(0)
  for i, m in ipairs(ms) do
    if m <= row then
      start = m
      stop = (ms[i + 1] or stop + 1) - 1
    end
  end
  if not start then
    return
  end
  local first = around and start or start + 1
  if first > stop then
    return
  end
  vim.cmd('normal! \27') -- leave any pending visual mode
  api.nvim_win_set_cursor(0, { first, 0 })
  vim.cmd('normal! V')
  api.nvim_win_set_cursor(0, { stop, 0 })
end

local commands = {
  { 'NbvRun', 'run' },
  { 'NbvRunAdvance', 'run_advance' },
  { 'NbvRunAll', 'run_all' },
  { 'NbvRunAbove', 'run_above' },
  { 'NbvInterrupt', 'interrupt' },
  { 'NbvRestart', 'restart' },
  { 'NbvCellDelete', 'cell_delete' },
  { 'NbvCellSplit', 'cell_split' },
  { 'NbvCellMerge', 'cell_merge' },
}

local keymaps = {
  { 'n', '<localleader>x', '<Cmd>NbvRun<CR>' },
  { 'n', '<S-CR>', '<Cmd>NbvRunAdvance<CR>' },
  { 'i', '<S-CR>', '<Esc><Cmd>NbvRunAdvance<CR>' },
  { 'n', '<localleader><CR>', '<Cmd>NbvRunAdvance<CR>' },
  { 'n', '<localleader>X', '<Cmd>NbvRunAll<CR>' },
  { 'n', '<localleader>ba', '<Cmd>NbvRunAbove<CR>' },
  { 'n', '<localleader>i', '<Cmd>NbvInterrupt<CR>' },
  { 'n', '<localleader>R', '<Cmd>NbvRestart<CR>' },
  { 'n', '<localleader>o', '<Cmd>NbvCellAdd<CR>' },
  { 'n', '<localleader>O', '<Cmd>NbvCellAdd!<CR>' },
  { 'n', '<localleader>dd', '<Cmd>NbvCellDelete<CR>' },
  { 'n', '<localleader>s', '<Cmd>NbvCellSplit<CR>' },
  { 'n', '<localleader>m', '<Cmd>NbvCellMerge<CR>' },
  { 'n', '<localleader>k', '<Cmd>NbvCellMove up<CR>' },
  { 'n', '<localleader>j', '<Cmd>NbvCellMove down<CR>' },
  { 'n', '<localleader>tc', '<Cmd>NbvCellType code<CR>' },
  { 'n', '<localleader>tm', '<Cmd>NbvCellType markdown<CR>' },
  { 'n', '<localleader>tr', '<Cmd>NbvCellType raw<CR>' },
  { 'n', '<localleader>c', '<Cmd>NbvClearOutput<CR>' },
}

local function fail(msg)
  error('nbv: ' .. msg, 0)
end

--- BufWriteCmd (§9.2): commit projection → canonical document → .ipynb.
local function on_write(ev)
  local buf = ev.buf
  if vim.fn.fnamemodify(ev.match, ':p') ~= api.nvim_buf_get_name(buf) then
    fail('the notebook buffer can only be written to its notebook (use :w)')
  end
  api.nvim_exec_autocmds('BufWritePre', { buffer = buf, modeline = false })
  local lines = api.nvim_buf_get_lines(buf, 0, -1, false)
  local res = vim.rpcrequest(M.chan, 'nbv_commit', api.nvim_buf_get_changedtick(buf), vim.v.cmdbang == 1, lines)
  if res.error then
    fail(res.error)
  end
  vim.bo[buf].modified = false
  api.nvim_exec_autocmds('BufWritePost', { buffer = buf, modeline = false })
  api.nvim_echo({ { res.message } }, false, {})
end

--- Replaces the buffer's text without an undo step, e.g. for the initial load.
local function set_text(buf, lines, undoable)
  local ul = vim.bo[buf].undolevels
  if not undoable then
    vim.bo[buf].undolevels = -1
  end
  api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  if not undoable then
    vim.bo[buf].undolevels = ul
  end
  vim.bo[buf].modified = false
end

local function setup_buffer(buf, filetype)
  vim.bo[buf].swapfile = false
  vim.bo[buf].buftype = ''
  local group = api.nvim_create_augroup('nbv_buffer', { clear = true })
  api.nvim_create_autocmd('BufWriteCmd', { group = group, buffer = buf, callback = on_write })
  api.nvim_create_autocmd({ 'FileWriteCmd', 'FileAppendCmd' }, {
    group = group,
    buffer = buf,
    callback = function()
      fail('partial writes of the notebook buffer are not supported')
    end,
  })
  api.nvim_create_autocmd('InsertLeave', {
    group = group,
    buffer = buf,
    callback = function()
      notify('normalise')
    end,
  })

  for _, c in ipairs(commands) do
    api.nvim_buf_create_user_command(buf, c[1], function()
      notify(c[2])
    end, {})
  end
  api.nvim_buf_create_user_command(buf, 'NbvCellAdd', function(o)
    notify('cell_add', { above = o.bang })
  end, { bang = true })
  api.nvim_buf_create_user_command(buf, 'NbvCellMove', function(o)
    notify('cell_move', { dir = o.args })
  end, { nargs = 1, complete = function() return { 'up', 'down' } end })
  api.nvim_buf_create_user_command(buf, 'NbvCellType', function(o)
    notify('cell_type', { kind = o.args })
  end, { nargs = 1, complete = function() return { 'code', 'markdown', 'raw' } end })
  api.nvim_buf_create_user_command(buf, 'NbvClearOutput', function(o)
    notify('clear_output', { all = o.bang })
  end, { bang = true })

  local map = function(mode, lhs, rhs, desc)
    vim.keymap.set(mode, lhs, rhs, { buffer = buf, silent = true, desc = desc })
  end
  map({ 'n', 'x', 'o' }, ']c', function() M.jump(1) end, 'Next cell')
  map({ 'n', 'x', 'o' }, '[c', function() M.jump(-1) end, 'Previous cell')
  map({ 'x', 'o' }, 'ic', function() M.select(false) end, 'Inside cell')
  map({ 'x', 'o' }, 'ac', function() M.select(true) end, 'Around cell')
  if not vim.g.nbv_no_default_keymaps then
    for _, k in ipairs(keymaps) do
      map(k[1], k[2], k[3])
    end
  end
  vim.bo[buf].filetype = filetype
end

--- BufReadCmd (§9.5): the first load and every :e / :e! read the .ipynb through Rust.
local function on_read(ev)
  local res = vim.rpcrequest(M.chan, 'nbv_load', ev.buf)
  if res.error then
    fail(res.error)
  end
  -- The first load is not undoable; a reload is, so undo can step back across it.
  local first = vim.b[ev.buf].nbv_loaded == nil
  set_text(ev.buf, res.lines, not first)
  vim.b[ev.buf].nbv_loaded = true
  setup_buffer(ev.buf, res.filetype)
  -- Sent after the text is in place: Rust (re)attaches and resyncs from here.
  vim.rpcnotify(M.chan, 'nbv', 'loaded', { buf = ev.buf })
end

--- Applies line edits if the buffer is still at `tick` and not in insert mode (§7.3).
--- Edits are ordered bottom-up. Returns whether they were applied.
function M.apply(buf, tick, edits, join, cursor)
  if api.nvim_buf_get_changedtick(buf) ~= tick then
    return false
  end
  if join and api.nvim_get_mode().mode:match('^[iR]') then
    return false
  end
  for i, e in ipairs(edits) do
    if join and i == 1 then
      pcall(vim.cmd, 'undojoin') -- refused right after an undo; then it is its own step
    end
    api.nvim_buf_set_lines(buf, e[1], e[2], false, e[3])
  end
  if cursor and api.nvim_get_current_buf() == buf then
    local last = api.nvim_buf_line_count(buf)
    api.nvim_win_set_cursor(0, { math.max(1, math.min(cursor + 1, last)), 0 })
  end
  return true
end

--- Defines the placeholder highlight groups. `nocombine` gives each group an attribute,
--- so Neovim reports it as its own highlight (§11.2) while drawing nothing visible.
local function define_highlights()
  for i = 0, M.slots - 1 do
    api.nvim_set_hl(0, 'NbvOutputSlot' .. i, { nocombine = true })
  end
  api.nvim_set_hl(0, 'NbvMarker', { default = true, link = 'Comment' })
  api.nvim_set_hl(0, 'NbvMarkerMarkdown', { default = true, link = 'Title' })
  api.nvim_set_hl(0, 'NbvStatusOk', { default = true, link = 'DiagnosticOk' })
  api.nvim_set_hl(0, 'NbvStatusError', { default = true, link = 'DiagnosticError' })
  api.nvim_set_hl(0, 'NbvStatusRunning', { default = true, link = 'DiagnosticWarn' })
  api.nvim_set_hl(0, 'NbvStatusQueued', { default = true, link = 'DiagnosticInfo' })
  api.nvim_set_hl(0, 'NbvStatusStale', { default = true, link = 'DiagnosticHint' })
end

--- Redraws all nbv decorations: output placeholders (virt_lines of the right height) and
--- cell status (virt_text on marker lines). Each placeholder row is one chunk wider than
--- any window, so its highlight reaches the end of the row.
function M.render(buf, tick, outputs, marks)
  if not api.nvim_buf_is_valid(buf) then
    return
  end
  if api.nvim_buf_get_changedtick(buf) ~= tick then
    -- Rust has not seen the latest change yet; it re-renders once it has.
    vim.rpcnotify(M.chan, 'nbv', 'rerender', {})
    return
  end
  api.nvim_buf_clear_namespace(buf, M.ns, 0, -1)
  local count = api.nvim_buf_line_count(buf)
  local pad = string.rep(' ', math.max(vim.o.columns, 80) + 8)
  for _, o in ipairs(outputs) do
    -- Each row carries its index as `#n#`: placeholder cells are drawn transparent, so the
    -- tag is never seen, but it tells Rust exactly which output row landed where.
    local rows = {}
    local group = 'NbvOutputSlot' .. o.slot
    for r = 1, o.height do
      rows[r] = { { ('#%d#'):format(r - 1) .. pad, group } }
    end
    local line = math.min(o.line, count - 1)
    pcall(api.nvim_buf_set_extmark, buf, M.ns, line, 0, {
      virt_lines = rows,
      virt_lines_above = o.above,
    })
  end
  for _, m in ipairs(marks) do
    if m.line < count then
      pcall(api.nvim_buf_set_extmark, buf, M.ns, m.line, 0, {
        virt_text = m.text,
        virt_text_pos = 'eol',
        line_hl_group = m.line_hl,
        hl_mode = 'combine',
      })
    end
  end
end

--- Asks the user for a line on behalf of the kernel (`input()`), without blocking RPC.
function M.input(prompt, password)
  vim.schedule(function()
    local ok, value = pcall(password and vim.fn.inputsecret or vim.fn.input, { prompt = prompt, cancelreturn = '' })
    vim.rpcnotify(M.chan, 'nbv', 'input', { value = ok and value or '' })
  end)
end

--- Pre-config: runs before the user's init.lua.
function M.pre(chan, path)
  M.chan = chan
  M.path = path
  vim.g.nbv = true
  local group = api.nvim_create_augroup('nbv', { clear = true })
  api.nvim_create_autocmd('BufReadCmd', {
    group = group,
    -- Autocmd patterns treat these as wildcards; the path must match literally.
    pattern = (path:gsub('([*?%[%]{},\\])', '\\%1')),
    callback = on_read,
  })
  api.nvim_create_autocmd('ColorScheme', { group = group, callback = define_highlights })
  -- Post-config: after the user's config, so these win.
  api.nvim_create_autocmd('VimEnter', {
    group = group,
    once = true,
    callback = function()
      define_highlights()
      vim.rpcnotify(M.chan, 'nbv', 'ready', { buf = vim.fn.bufnr(path) })
    end,
  })
end

package.loaded['nbv'] = M
return M
