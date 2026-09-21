-- nbv: the Lua companion loaded into the embedded Neovim (R6).
--
-- The notebook is drawn by Rust. Neovim contributes one transparent "home" window, through
-- which the notebook shows, and one floating window per visible cell, each editing that
-- cell's own buffer. This file creates and places those windows and buffers on Rust's
-- instruction, intercepts writes and reloads, and reports focus changes. It holds no notebook
-- state: windows and buffers carry their cell's key (`w:nbv_key`, `b:nbv_key`), and
-- everything else lives in Rust. Rust loads this file before the user's config (pre-config);
-- it finishes its setup on VimEnter (post-config).

local M = {}
local api = vim.api

--- Channel of the nbv process, set at load.
M.chan = nil
--- Name of the home buffer (`nbv://<notebook path>`), set at load.
M.home_name = nil
M.home_buf = nil
M.home_win = nil
--- The latest focus request from Rust, echoed in focus notifications so Rust can discard
--- ones that predate its own requests.
M.focus_seq = 0

local function notify(action, args)
  vim.rpcnotify(M.chan, 'nbv', action, args or vim.empty_dict())
end

local function fail(msg)
  error('nbv: ' .. msg, 0)
end

--- The cell a window edits, if it is a cell window still showing its cell's buffer.
local function window_key(win)
  local key = vim.w[win].nbv_key
  if key and vim.b[api.nvim_win_get_buf(win)].nbv_key == key then
    return key
  end
end

local function viewport()
  local w = M.home_win
  if not (w and api.nvim_win_is_valid(w)) then
    return nil
  end
  local pos = api.nvim_win_get_position(w)
  return { row = pos[1], col = pos[2], width = api.nvim_win_get_width(w), height = api.nvim_win_get_height(w) }
end

local function report_focus()
  local win = api.nvim_get_current_win()
  notify('focus', { seq = M.focus_seq, home = win == M.home_win, key = window_key(win) })
end

-- Highlight groups. Rust draws the notebook with the colours these resolve to.
local THEME = {
  border = 'NbvBorder',
  nav = 'NbvBorderNav',
  edit = 'NbvBorderEdit',
  header = 'NbvHeader',
  dim = 'NbvDim',
  stderr = 'NbvStderr',
  ok = 'NbvStatusOk',
  error = 'NbvStatusError',
  running = 'NbvStatusRunning',
  queued = 'NbvStatusQueued',
  stale = 'NbvStatusStale',
}

local function theme()
  local t = {}
  for name, group in pairs(THEME) do
    local h = api.nvim_get_hl(0, { name = group, link = false })
    t[name] = { fg = h.fg, bg = h.bg }
  end
  return t
end

local function define_highlights()
  -- The special colour marks the home window's cells as transparent (§11.2); `nocombine`
  -- keeps it from mixing into anything drawn over it.
  api.nvim_set_hl(0, 'NbvTransparent', { sp = M.transparent_sp, nocombine = true })
  local links = {
    NbvBorder = 'LineNr',
    NbvBorderNav = 'DiagnosticInfo',
    NbvBorderEdit = 'DiagnosticOk',
    NbvHeader = 'Title',
    NbvDim = 'Comment',
    NbvStderr = 'DiagnosticError',
    NbvStatusOk = 'DiagnosticOk',
    NbvStatusError = 'DiagnosticError',
    NbvStatusRunning = 'DiagnosticWarn',
    NbvStatusQueued = 'DiagnosticInfo',
    NbvStatusStale = 'DiagnosticHint',
  }
  for group, link in pairs(links) do
    api.nvim_set_hl(0, group, { default = true, link = link })
  end
end

-- Every group the home window can draw with maps to NbvTransparent, so even decorations a
-- plugin turns on there (line numbers, signs, a cursorline) stay invisible.
local HOME_GROUPS = {
  'Normal', 'NormalNC', 'EndOfBuffer', 'CursorLine', 'CursorColumn', 'ColorColumn', 'LineNr',
  'LineNrAbove', 'LineNrBelow', 'CursorLineNr', 'SignColumn', 'FoldColumn', 'NonText',
  'Whitespace', 'Folded', 'Visual', 'Conceal',
}

local function set_local(win, name, value)
  api.nvim_set_option_value(name, value, { win = win, scope = 'local' })
end

local function setup_home_window(win)
  if not (win and api.nvim_win_is_valid(win)) then
    return
  end
  local hl = {}
  for _, g in ipairs(HOME_GROUPS) do
    hl[#hl + 1] = g .. ':NbvTransparent'
  end
  local opts = {
    winhighlight = table.concat(hl, ','),
    number = false,
    relativenumber = false,
    signcolumn = 'no',
    foldcolumn = '0',
    statuscolumn = '',
    cursorline = false,
    cursorcolumn = false,
    colorcolumn = '',
    list = false,
    spell = false,
    fillchars = 'eob: ',
  }
  for name, value in pairs(opts) do
    set_local(win, name, value)
  end
end

-- New windows copy the current window's options, which is the home window's transparency.
-- Cell windows take the user's global values instead.
local CELL_OPTIONS = {
  'number', 'relativenumber', 'signcolumn', 'foldcolumn', 'statuscolumn', 'cursorcolumn',
  'colorcolumn', 'list', 'spell', 'fillchars',
}

local function setup_cell_window(win, key)
  for _, name in ipairs(CELL_OPTIONS) do
    set_local(win, name, api.nvim_get_option_value(name, { scope = 'global' }))
  end
  local user = api.nvim_get_option_value('winhighlight', { scope = 'global' })
  set_local(win, 'winhighlight', 'NormalFloat:Normal,FloatBorder:Normal' .. (user ~= '' and (',' .. user) or ''))
  -- The window is exactly as tall as its cell has lines; wrapped lines would not fit.
  set_local(win, 'wrap', false)
  vim.w[win].nbv_key = key
end

--- The cursorline shows only in the cell being edited, if the user has it on.
local function set_active(win, active)
  set_local(win, 'cursorline', active and api.nvim_get_option_value('cursorline', { scope = 'global' }))
end

--- BufWriteCmd (§9.2): commit every cell buffer → canonical document → .ipynb.
local function on_write(ev)
  local buf = ev.buf
  local name = api.nvim_buf_get_name(buf)
  if ev.match ~= name and vim.fn.fnamemodify(ev.match, ':p') ~= name then
    fail('notebook buffers can only be written to their notebook (use :w)')
  end
  api.nvim_exec_autocmds('BufWritePre', { buffer = buf, modeline = false })
  local buffers = {}
  for _, b in ipairs(api.nvim_list_bufs()) do
    if vim.b[b].nbv_key then
      buffers[#buffers + 1] = { b, api.nvim_buf_get_lines(b, 0, -1, false) }
    end
  end
  local res = vim.rpcrequest(M.chan, 'nbv_commit', vim.v.cmdbang == 1, buffers)
  if res.error then
    fail(res.error)
  end
  for _, b in ipairs(api.nvim_list_bufs()) do
    if b == M.home_buf or vim.b[b].nbv_key then
      vim.bo[b].modified = false
    end
  end
  api.nvim_exec_autocmds('BufWritePost', { buffer = buf, modeline = false })
  api.nvim_echo({ { res.message } }, false, {})
end

--- BufReadCmd (§9.5): `:e` / `:e!` on any notebook buffer reloads the .ipynb through Rust.
local function on_reload()
  local res = vim.rpcrequest(M.chan, 'nbv_reload')
  if res.error then
    fail(res.error)
  end
  if M.home_buf and api.nvim_buf_is_valid(M.home_buf) then
    vim.bo[M.home_buf].modified = false
  end
end

local function intercept_io(buf)
  local group = api.nvim_create_augroup('nbv_buffer_' .. buf, { clear = true })
  api.nvim_create_autocmd('BufWriteCmd', { group = group, buffer = buf, callback = on_write })
  api.nvim_create_autocmd({ 'FileWriteCmd', 'FileAppendCmd' }, {
    group = group,
    buffer = buf,
    callback = function()
      fail('partial writes of notebook buffers are not supported')
    end,
  })
  api.nvim_create_autocmd('BufReadCmd', { group = group, buffer = buf, callback = on_reload })
end

--- Sends an action to Rust on behalf of the current window's cell, if any.
function M.act(action, extra)
  local args = extra or {}
  local win = api.nvim_get_current_win()
  args.key = window_key(win)
  args.line = api.nvim_win_get_cursor(win)[1] - 1
  notify(action, args)
end

--- Replaces a buffer's text without an undo step, e.g. for the initial load.
local function set_initial_text(buf, lines)
  local ul = vim.bo[buf].undolevels
  vim.bo[buf].undolevels = -1
  api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].undolevels = ul
  vim.bo[buf].modified = false
end

local function create_buffer(key, spec)
  local buf = api.nvim_create_buf(false, false)
  -- An ordinary buffer (buftype=""), so LSP attaches (§9.1). Nothing is ever written to its name.
  api.nvim_buf_set_name(buf, spec.name)
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'hide'
  set_initial_text(buf, spec.lines)
  vim.b[buf].nbv_key = key
  intercept_io(buf)
  -- The one way out of a cell: <Esc> in Normal mode, which otherwise does nothing. It also
  -- clears search highlighting, the usual job of a user's own <Esc> mapping.
  vim.keymap.set('n', '<Esc>', function()
    vim.cmd.nohlsearch()
    M.leave()
  end, { buffer = buf, silent = true, desc = 'nbv: leave the cell' })
  -- Running from inside the cell. As mappings, they apply after every edit typed before them.
  vim.keymap.set({ 'n', 'i' }, '<S-CR>', function()
    M.act('run_advance')
  end, { buffer = buf, silent = true, desc = 'nbv: run the cell and go to the next' })
  vim.keymap.set({ 'n', 'i' }, '<C-CR>', function()
    M.act('run')
  end, { buffer = buf, silent = true, desc = 'nbv: run the cell' })
  return buf
end

--- Places a window for every listed cell and closes the rest (§11.1). Each cell is
--- `{ key, row, col, width, height, topline, create? }`; `create` makes the cell's buffer.
--- Ends with a redraw, then tells Rust the layout is on screen, so Rust always draws the
--- notebook around the windows as Neovim has them.
function M.layout(seq, spec)
  local ok, err = pcall(function()
    setup_home_window(M.home_win)
    local bufs = {}
    for _, b in ipairs(api.nvim_list_bufs()) do
      local k = vim.b[b].nbv_key
      if k then
        bufs[k] = b
      end
    end
    local want = {}
    for _, c in ipairs(spec.cells) do
      want[c.key] = true
    end
    local wins = {}
    local current = api.nvim_get_current_win()
    for _, w in ipairs(api.nvim_list_wins()) do
      local k = vim.w[w].nbv_key
      if k then
        if want[k] and not wins[k] and api.nvim_win_get_buf(w) == bufs[k] then
          wins[k] = w
        elseif w ~= current then
          api.nvim_win_close(w, true)
        end
      end
    end
    local created = {}
    for _, c in ipairs(spec.cells) do
      local buf = bufs[c.key]
      if not buf and c.create then
        buf = create_buffer(c.key, c.create)
        created[#created + 1] = { key = c.key, buf = buf, filetype = c.create.filetype }
      end
      if buf then
        local active = c.key == spec.active
        local cfg = {
          relative = 'editor',
          row = c.row,
          col = c.col,
          width = c.width,
          height = c.height,
          focusable = active,
          zindex = 1,
          hide = false,
        }
        local w = wins[c.key]
        if w then
          api.nvim_win_set_config(w, cfg)
        else
          cfg.border = 'none'
          w = api.nvim_open_win(buf, false, cfg)
          setup_cell_window(w, c.key)
          wins[c.key] = w
        end
        set_active(w, active)
        if c.topline > 0 then
          api.nvim_win_call(w, function()
            vim.fn.winrestview({ topline = c.topline })
          end)
        end
      end
    end
    -- Set in the cell's window, so ftplugins' window-local settings land there.
    for _, n in ipairs(created) do
      api.nvim_win_call(wins[n.key], function()
        vim.bo[n.buf].filetype = n.filetype
      end)
      notify('buffer', { key = n.key, buf = n.buf })
    end
  end)
  vim.cmd.redraw()
  notify('layout_done', { seq = seq, error = (not ok) and tostring(err) or nil, viewport = viewport() })
end

--- Focuses a cell's window (made focusable by the layout that precedes this call).
function M.enter(seq, key, insert)
  M.focus_seq = seq
  for _, w in ipairs(api.nvim_list_wins()) do
    if window_key(w) == key then
      api.nvim_win_set_config(w, { focusable = true })
      set_active(w, true)
      api.nvim_set_current_win(w)
      if insert then
        vim.cmd.startinsert()
      end
      return
    end
  end
  report_focus()
end

--- Returns to the home window. Rust queues this as input, after `<C-\><C-n>`, with its
--- latest focus request; `<Esc>` in a cell calls it without one.
function M.leave(seq)
  M.focus_seq = seq or M.focus_seq
  if M.home_win and api.nvim_win_is_valid(M.home_win) and api.nvim_get_current_win() ~= M.home_win then
    api.nvim_set_current_win(M.home_win)
  else
    report_focus()
  end
end

--- Rewrites a cell buffer after a structural change (split, merge, undo). Undoable in that
--- buffer, like any edit.
function M.set_text(buf, lines)
  if api.nvim_buf_is_valid(buf) then
    api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  end
end

function M.wipe(bufs)
  for _, b in ipairs(bufs) do
    if api.nvim_buf_is_valid(b) then
      api.nvim_buf_delete(b, { force = true })
    end
  end
end

--- Marks the notebook changed in ways no cell buffer shows: structure, outputs.
function M.set_modified()
  if M.home_buf and api.nvim_buf_is_valid(M.home_buf) then
    vim.bo[M.home_buf].modified = true
  end
end

--- Opens `lines` in a centred float with focus, read-only, moved through with the usual
--- motions. Returns the buffer and a function that closes the float and returns home.
local function open_list(lines, title)
  local buf = api.nvim_create_buf(false, true)
  api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].modifiable = false
  vim.bo[buf].bufhidden = 'wipe'
  local w = vim.fn.strdisplaywidth(title) + 4
  for _, l in ipairs(lines) do
    w = math.max(w, vim.fn.strdisplaywidth(l) + 1)
  end
  w = math.min(w, vim.o.columns - 4)
  local h = math.min(#lines, vim.o.lines - 4)
  local win = api.nvim_open_win(buf, true, {
    relative = 'editor',
    row = math.floor((vim.o.lines - h) / 2) - 1,
    col = math.floor((vim.o.columns - w) / 2),
    width = w,
    height = h,
    border = 'rounded',
    title = title,
    title_pos = 'center',
    style = 'minimal',
  })
  -- Not the home window's transparency, which a new window copies.
  set_local(win, 'winhighlight', '')
  return buf, win, function()
    api.nvim_win_close(win, true)
    M.leave()
  end
end

--- Offers kernels in a list: the usual motions move, `<CR>` picks, `q` cancels.
function M.pick_kernel(items)
  local lines = {}
  for i, item in ipairs(items) do
    lines[i] = ' ' .. item
  end
  local buf, win, close = open_list(lines, ' Kernel · <CR> picks · q cancels ')
  set_local(win, 'cursorline', true)
  local opts = { buffer = buf, nowait = true, silent = true }
  vim.keymap.set('n', '<CR>', function()
    local index = api.nvim_win_get_cursor(win)[1]
    close()
    notify('kernel_chosen', { index = index })
  end, vim.tbl_extend('force', opts, { desc = 'nbv: pick this kernel' }))
  vim.keymap.set('n', 'q', close, vim.tbl_extend('force', opts, { desc = 'nbv: cancel' }))
end

--- Asks for the notebook's new file name, starting from the current one.
function M.rename(name)
  vim.schedule(function()
    vim.ui.input({ prompt = 'Rename: ', default = name }, function(value)
      if value and value ~= '' and value ~= name then
        notify('rename', { name = value })
      end
    end)
  end)
end

--- Gives the home buffer the renamed notebook's name, and drops the alternate buffer
--- renaming leaves under the old one.
function M.rename_home(name)
  local old = api.nvim_buf_get_name(M.home_buf)
  api.nvim_buf_set_name(M.home_buf, name)
  M.home_name = name
  for _, b in ipairs(api.nvim_list_bufs()) do
    if b ~= M.home_buf and api.nvim_buf_get_name(b) == old then
      api.nvim_buf_delete(b, { force = true })
    end
  end
end

--- Shows the key list in a float: `sections` is `{ { title, { { keys, description }, … } }, … }`.
--- `q`, `<Esc>` or `?` closes it.
function M.help(sections)
  local lines, marks = {}, {}
  local width = 0
  for _, s in ipairs(sections) do
    for _, k in ipairs(s[2]) do
      width = math.max(width, vim.fn.strdisplaywidth(k[1]))
    end
  end
  for i, s in ipairs(sections) do
    if i > 1 then
      lines[#lines + 1] = ''
    end
    lines[#lines + 1] = ' ' .. s[1]
    marks[#marks + 1] = { #lines - 1, 0, -1, 'Title' }
    for _, k in ipairs(s[2]) do
      if k[1] == '' then
        -- A row without a key is prose, not a key column.
        lines[#lines + 1] = '   ' .. k[2]
      else
        local pad = string.rep(' ', width - vim.fn.strdisplaywidth(k[1]))
        lines[#lines + 1] = '   ' .. k[1] .. pad .. '  ' .. k[2]
        marks[#marks + 1] = { #lines - 1, 3, 3 + #k[1], 'Special' }
      end
    end
  end
  local buf, _, close = open_list(lines, ' nbv keys · q closes ')
  local ns = api.nvim_create_namespace('nbv_help')
  for _, m in ipairs(marks) do
    local stop = m[3] == -1 and #lines[m[1] + 1] or m[3]
    api.nvim_buf_set_extmark(buf, ns, m[1], m[2], { end_col = stop, hl_group = m[4] })
  end
  for _, key in ipairs({ 'q', '<Esc>', '?' }) do
    vim.keymap.set('n', key, close, { buffer = buf, nowait = true, silent = true, desc = 'nbv: close the key list' })
  end
end

--- Asks the user for a line on behalf of the kernel (`input()`), without blocking RPC.
function M.input(prompt, password)
  vim.schedule(function()
    local ok, value = pcall(password and vim.fn.inputsecret or vim.fn.input, { prompt = prompt, cancelreturn = '' })
    vim.rpcnotify(M.chan, 'nbv', 'input', { value = ok and value or '' })
  end)
end

-- Only actions without a navigation key have commands.
local commands = {
  { 'NbvRunAll', 'run_all' },
  { 'NbvRunAbove', 'run_above' },
  { 'NbvSplit', 'split' },
}

--- The first read of the home buffer, at startup. Later reads are reloads (intercept_io).
local function on_read_home(ev)
  local buf = ev.buf
  vim.bo[buf].buftype = 'acwrite'
  vim.bo[buf].swapfile = false
  vim.bo[buf].bufhidden = 'hide'
  vim.bo[buf].buflisted = false
  vim.bo[buf].modifiable = false
  vim.bo[buf].modified = false
  intercept_io(buf)
  vim.bo[buf].filetype = 'nbv'
end

--- Post-config: runs after the user's config, so these settings win.
local function post()
  define_highlights()
  M.home_buf = vim.fn.bufnr(M.home_name)
  M.home_win = vim.fn.bufwinid(M.home_buf)
  if M.home_win == -1 then
    M.home_win = api.nvim_get_current_win()
  end
  -- Floating cell windows have no statuslines of their own; a global one follows them.
  if vim.o.laststatus ~= 0 then
    vim.o.laststatus = 3
  end
  setup_home_window(M.home_win)

  local group = api.nvim_create_augroup('nbv_post', { clear = true })
  api.nvim_create_autocmd('WinEnter', { group = group, callback = report_focus })
  api.nvim_create_autocmd({ 'WinResized', 'VimResized' }, {
    group = group,
    callback = function()
      notify('viewport', viewport())
    end,
  })

  for _, c in ipairs(commands) do
    api.nvim_create_user_command(c[1], function()
      M.act(c[2])
    end, {})
  end
  api.nvim_create_user_command('NbvClearOutput', function(o)
    M.act('clear_output', { all = o.bang })
  end, { bang = true })

  notify('ready', { home = M.home_buf, viewport = viewport(), theme = theme() })
end

--- Pre-config: runs before the user's init.lua.
function M.pre(chan, home_name, transparent_sp)
  M.chan = chan
  M.home_name = home_name
  M.transparent_sp = transparent_sp
  vim.g.nbv = true
  local group = api.nvim_create_augroup('nbv', { clear = true })
  api.nvim_create_autocmd('BufReadCmd', {
    group = group,
    once = true,
    -- Autocmd patterns treat these as wildcards; the name must match literally.
    pattern = (home_name:gsub('([*?%[%]{},\\])', '\\%1')),
    callback = on_read_home,
  })
  api.nvim_create_autocmd('ColorScheme', {
    group = group,
    callback = function()
      define_highlights()
      notify('theme', theme())
    end,
  })
  api.nvim_create_autocmd('VimEnter', { group = group, once = true, callback = post })
end

package.loaded['nbv'] = M
return M
