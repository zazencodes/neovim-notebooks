-- Notebook LSP transport. The server owns one ordinary Python document; Neovim owns
-- ordinary cell buffers. Only line offsets change, preserving negotiated UTF character
-- offsets exactly. No disk projection and no global LSP handler overrides.
local M = {}
local api = vim.api
local document, factory, server, client_id
local sessions = {}
local server_config, starting
local generation, membership = 0, nil
local enabled_callback
local watched = {}

local function buffers()
  local by_key = {}
  for _, b in ipairs(api.nvim_list_bufs()) do
    local key = vim.b[b].nbv_key
    if key then by_key[key] = b end
  end
  return by_key
end

local refresh_pending = false
local function schedule_refresh()
  if refresh_pending then return end
  refresh_pending = true
  vim.schedule(function()
    refresh_pending = false
    if not document then return end
    for _, session in ipairs(sessions) do
      if not session.rpc.is_closing() then session.refresh() end
    end
  end)
end

local function snapshot()
  local s = { cells = {}, by_uri = {}, lines = {}, uri = vim.uri_from_fname(document.name) }
  local by_key = buffers()
  for _, cell in ipairs(document.cells) do
    local b = by_key[cell.key]
    local lines = b and api.nvim_buf_get_lines(b, 0, -1, false) or cell.lines
    local c = { uri = b and vim.uri_from_bufnr(b) or vim.uri_from_fname(cell.name),
      start = #s.lines, count = #lines, buf = b, source = table.concat(lines, '\n') }
    s.cells[#s.cells + 1] = c
    s.by_uri[c.uri] = c
    vim.list_extend(s.lines, lines)
    -- A real blank separator, never mapped to a cell or exposed as an edit target.
    s.lines[#s.lines + 1] = ''
  end
  s.text = table.concat(s.lines, '\n')
  local identity = {}
  for _, c in ipairs(s.cells) do
    identity[#identity + 1] = { c.uri, c.start, c.count, c.buf or false }
  end
  s.identity = vim.json.encode(identity)
  return s
end

local function local_cell(s, uri)
  return s.by_uri[uri]
end

local function projected_cell(s, line)
  local low, high = 1, #s.cells
  while low <= high do
    local mid = math.floor((low + high) / 2)
    local c = s.cells[mid]
    if line < c.start then high = mid - 1
    elseif line >= c.start + c.count then low = mid + 1
    else return c end
  end
end

local function local_range(c, range, diagnostic)
  if not range then return nil end
  assert(range.start.line >= c.start and range.start.line < c.start + c.count,
    'nbv: server range starts outside the requested cell')
  local r = vim.deepcopy(range)
  if r['end'].line >= c.start + c.count then
    if not diagnostic then error('nbv: server edit/range crosses a cell boundary') end
    r['end'] = { line = c.start + c.count - 1, character = 0 }
    if r['end'].line == r.start.line then r['end'].character = r.start.character end
  end
  r.start.line = r.start.line - c.start
  r['end'].line = r['end'].line - c.start
  return r
end

local function edit(s, e)
  local c = projected_cell(s, e.range.start.line)
  assert(c, 'nbv: server edit targets a synthetic separator')
  local r = vim.deepcopy(e)
  r.range = local_range(c, e.range)
  return c, r
end

local function workspace(s, value)
  local result = vim.deepcopy(value)
  if value.changes then
    result.changes = {}
    for uri, edits in pairs(value.changes) do
      if uri == s.uri then
        for _, e in ipairs(edits) do
          local c, mapped = edit(s, e)
          result.changes[c.uri] = result.changes[c.uri] or {}
          table.insert(result.changes[c.uri], mapped)
        end
      else result.changes[uri] = edits end
    end
  end
  if value.documentChanges then
    result.documentChanges = {}
    for _, change in ipairs(value.documentChanges) do
      if change.textDocument and change.textDocument.uri == s.uri then
        local v = change.textDocument.version
        assert(v == nil or v == vim.NIL or v == s.version, 'nbv: server workspace edit uses a stale notebook version')
        local groups = {}
        for _, e in ipairs(change.edits) do
          local c, mapped = edit(s, e)
          if not groups[c.uri] then
            groups[c.uri] = { textDocument = { uri = c.uri, version = vim.NIL }, edits = {} }
            table.insert(result.documentChanges, groups[c.uri])
          end
          table.insert(groups[c.uri].edits, mapped)
        end
      else
        assert(change.uri ~= s.uri and change.oldUri ~= s.uri and change.newUri ~= s.uri,
          'nbv: server cannot rename/delete the synthetic notebook document')
        table.insert(result.documentChanges, change)
      end
    end
  end
  return result
end

-- Response ranges without a URI (completion/hover/text edits) refer to the request cell.
-- LocationLink origin ranges refer to that cell, target ranges to targetUri.
local function response(s, value, origin)
  if type(value) ~= 'table' or value == vim.NIL then return value end
  if value.changes or value.documentChanges then return workspace(s, value) end
  if value.start and value['end'] and origin then
    return local_range(origin, value)
  end
  local r = vim.deepcopy(value)
  local target = value.uri or value.targetUri
  if target and target ~= s.uri then
    if value.originSelectionRange and origin then
      r.originSelectionRange = local_range(origin, value.originSelectionRange)
    end
    return r
  end
  local c = target == s.uri and projected_cell(s, (value.range or value.targetSelectionRange).start.line)
    or (not target and origin)
  if target == s.uri and not c then return nil end
  if target == s.uri then
    if value.uri then r.uri = c.uri else r.targetUri = c.uri end
  end
  for k, v in pairs(value) do
    if k == 'data' then -- Server-private, opaque payloads must round-trip unchanged.
    elseif k == 'originSelectionRange' and origin then
      r[k] = local_range(origin, v)
    elseif (k == 'range' or k == 'insert' or k == 'replace' or k == 'targetRange'
      or k == 'targetSelectionRange') and c then
      r[k] = local_range(c, v, target ~= nil)
    elseif k == 'additionalTextEdits' then
      r[k] = {}
      for _, e in ipairs(v) do
        local dest, mapped = edit(s, e)
        mapped._nbv_uri = dest.uri
        mapped._nbv_source = dest.source
        table.insert(r[k], mapped)
      end
    elseif type(v) == 'table' then
      if vim.islist(v) then
        r[k] = {}
        for _, item in ipairs(v) do
          local mapped = response(s, item, origin)
          if mapped then table.insert(r[k], mapped) end
        end
      else r[k] = response(s, v, origin) end
    end
  end
  -- Completion resolve must receive the server's original coordinates and opaque data.
  if value.label and origin then
    r._nbv_item = { original = value, uri = origin.uri, text = s.text, identity = s.identity,
      cells = s.cells, position = s.completion_position }
  end
  return r
end

-- CompleteDone resolves after the accepted word has already changed the cell. Permit
-- precisely that edit, while rejecting changed membership, other cells, or unrelated text.
local function accepted_completion(item, latest, encoding)
  if item.text == latest.text and item.identity == latest.identity then return true end
  if #item.cells ~= #latest.cells then return false end
  local before, after
  for i, c in ipairs(item.cells) do
    local next_cell = latest.cells[i]
    if c.uri ~= next_cell.uri or c.buf ~= next_cell.buf then return false end
    if c.uri == item.uri then before, after = c, next_cell
    elseif c.source ~= next_cell.source then return false end
  end
  if not before or not item.position then return false end
  local original = item.original
  local e = original.textEdit
  local range = e and (e.range or e.insert or e.replace)
  local lines = vim.split(before.source, '\n', { plain = true })
  if not range then
    local row = item.position.line - before.start
    local line = lines[row + 1]
    local col = vim.str_byteindex(line, encoding, item.position.character, false)
    local word = vim.fn.matchstr(line:sub(1, col), [[\k*$]])
    range = { start = { line = row + before.start,
      character = vim.str_utfindex(line, encoding, col - #word, false) }, ['end'] = item.position }
  end
  local first, last = range.start.line - before.start, range['end'].line - before.start
  if first < 0 or last >= #lines then return false end
  local start_col = vim.str_byteindex(lines[first + 1], encoding, range.start.character, false)
  local end_col = vim.str_byteindex(lines[last + 1], encoding, range['end'].character, false)
  local text = (e and e.newText) or original.insertText or original.label
  local function matches(replacement)
    local edited = {}
    for i = 1, first do edited[#edited + 1] = lines[i] end
    local replacement_lines = vim.split(replacement, '\n', { plain = true })
    replacement_lines[1] = lines[first + 1]:sub(1, start_col) .. replacement_lines[1]
    replacement_lines[#replacement_lines] = replacement_lines[#replacement_lines] .. lines[last + 1]:sub(end_col + 1)
    vim.list_extend(edited, replacement_lines)
    for i = last + 2, #lines do edited[#edited + 1] = lines[i] end
    return table.concat(edited, '\n') == after.source
  end
  -- Native completion inserts its displayed word before expanding snippets or applying
  -- multiline edits. Other consumers may already have applied the full LSP text edit.
  local word = original.insertTextFormat == 2 and original.label or text:match('[^\n]*')
  return matches(text) or matches(word)
end

local function transport(config)
  return function(dispatchers)
    local rpc, current, opened, version = nil, nil, false, 0
    local initialized, warned_unversioned = false, false
    local encoding = 'utf-16'
    local known = {}
    local function refresh()
      if not document then return nil end
      local next_snapshot = snapshot()
      if not initialized then return next_snapshot end
      if current and current.uri ~= next_snapshot.uri and opened then
        rpc.notify('textDocument/didClose', { textDocument = { uri = current.uri } })
        opened = false
      end
      if not opened then
        version = version + 1
        rpc.notify('textDocument/didOpen', { textDocument = {
          uri = next_snapshot.uri, languageId = 'python', version = version, text = next_snapshot.text,
        } })
        opened = true
      elseif current.text ~= next_snapshot.text or current.identity ~= next_snapshot.identity then
        if client_id then
          for _, pull in ipairs({ false, true }) do
            local ns = vim.lsp.diagnostic.get_namespace(client_id, pull)
            for _, c in ipairs(current.cells) do
              if c.buf and api.nvim_buf_is_valid(c.buf) then vim.diagnostic.reset(ns, c.buf) end
            end
          end
        end
        version = version + 1
        rpc.notify('textDocument/didChange', { textDocument = { uri = next_snapshot.uri, version = version },
          contentChanges = { { text = next_snapshot.text } } })
      end
      for _, c in ipairs(next_snapshot.cells) do known[c.uri] = true end
      next_snapshot.version = version
      current = next_snapshot
      return current
    end
    local wrapped = vim.tbl_extend('force', dispatchers, {
      notification = function(method, params)
        if method == 'textDocument/publishDiagnostics' and current and params.uri == current.uri then
          if not document then return end
          local latest = snapshot()
          if latest.text ~= current.text or latest.identity ~= current.identity then return end
          -- Require the exact document version: an unversioned response cannot be safely
          -- associated with a cell after structural changes. Pyright publishes versions.
          if params.version == nil then
            local client = client_id and vim.lsp.get_client_by_id(client_id)
            if not warned_unversioned and not (client and client:supports_method('textDocument/diagnostic')) then
              warned_unversioned = true
              vim.notify('nbv: this server publishes unversioned diagnostics; notebook diagnostics require '
                .. 'pull diagnostics or versioned publishDiagnostics', vim.log.levels.ERROR)
            end
            return
          end
          if params.version ~= version then return end
          local per_cell = {}
          for _, c in ipairs(current.cells) do per_cell[c.uri] = {} end
          for _, d in ipairs(params.diagnostics) do
            local c = projected_cell(current, d.range.start.line)
            if c then
              local mapped = vim.deepcopy(d)
              mapped.range = local_range(c, d.range, true)
              if d.relatedInformation then
                mapped.relatedInformation = response(current, d.relatedInformation)
              end
              table.insert(per_cell[c.uri], mapped)
            end
          end
          for uri, diagnostics in pairs(per_cell) do
            dispatchers.notification(method, { uri = uri, diagnostics = diagnostics })
          end
        else dispatchers.notification(method, params) end
      end,
      server_request = function(method, params)
        if method == 'workspace/applyEdit' then
          if not document then return { applied = false, failureReason = 'Notebook language changed' } end
          local ok, mapped = pcall(workspace, refresh(), params.edit)
          if not ok then return { applied = false, failureReason = tostring(mapped) } end
          params = vim.tbl_extend('force', params, { edit = mapped })
        end
        return dispatchers.server_request(method, params)
      end,
    })
    rpc = type(config.cmd) == 'function' and config.cmd(wrapped, config)
      or vim.lsp.rpc.start(config.cmd, wrapped,
        { cwd = config.cmd_cwd, env = config.cmd_env, detached = config.detached })
    local proxy = vim.tbl_extend('force', rpc, {})
    proxy.notify = function(method, params)
      if method == 'initialized' then
        local sent = rpc.notify(method, params)
        initialized = true
        refresh()
        return sent
      end
      local uri = params and params.textDocument and params.textDocument.uri
      if uri and document and known[uri] then
        if method == 'textDocument/didOpen' or method == 'textDocument/didChange'
          or method == 'textDocument/didClose' or method == 'textDocument/didSave' then
          -- on_lines coalesces live text updates, structural sync refreshes membership,
          -- and every request flushes the overlay. Per-cell sync never rebuilds it.
          return true
        end
      end
      return rpc.notify(method, params)
    end
    proxy.request = function(method, params, callback, notify_reply)
      if method == 'initialize' then
        -- This transport implements text documents, not notebookDocument synchronization.
        params.capabilities.notebookDocument = nil
      end
      local s, origin, stale_completion
      local p = vim.deepcopy(params)
      if params and params.textDocument and document then
        s = refresh()
        origin = local_cell(s, params.textDocument.uri)
        if origin then
          p.textDocument.uri = s.uri
          if p.position then p.position.line = p.position.line + origin.start end
          if p.range then
            p.range.start.line = p.range.start.line + origin.start
            p.range['end'].line = p.range['end'].line + origin.start
          end
        end
      elseif params and params._nbv_item then
        s = refresh()
        stale_completion = not s or not accepted_completion(params._nbv_item, s, encoding)
        origin = s and local_cell(s, params._nbv_item.uri)
        p = params._nbv_item.original
      end
      if not s and document and initialized and method ~= 'shutdown' then s = refresh() end
      if s and method == 'textDocument/completion' then s.completion_position = vim.deepcopy(p.position) end
      return rpc.request(method, p, function(err, result, id)
        if stale_completion then
          err, result = { code = -32801, message = 'Notebook changed since this completion was requested' }, nil
        end
        if method == 'initialize' and result then
          encoding = result.capabilities.positionEncoding or 'utf-16'
          -- Features whose result coordinates are not covered by this transport are absent,
          -- rather than silently returning shadow-document coordinates to plugins.
          for _, capability in ipairs({ 'semanticTokensProvider', 'documentFormattingProvider',
            'documentRangeFormattingProvider', 'documentOnTypeFormattingProvider', 'foldingRangeProvider',
            'inlayHintProvider', 'documentSymbolProvider', 'codeLensProvider', 'codeActionProvider',
            'selectionRangeProvider', 'documentHighlightProvider',
            'linkedEditingRangeProvider', 'callHierarchyProvider', 'typeHierarchyProvider' }) do
            result.capabilities[capability] = nil
          end
        end
        if s and not err then
          local latest = document and snapshot()
          if not latest or latest.text ~= s.text or latest.identity ~= s.identity then
            err, result = { code = -32801, message = 'Notebook changed during language-server request' }, nil
          else
            local ok, mapped = pcall(function()
              if method == 'textDocument/diagnostic' and origin and result and result.items then
                local report = vim.deepcopy(result)
                report.items = {}
                for _, d in ipairs(result.items) do
                  if projected_cell(s, d.range.start.line) == origin then
                    local mapped_diagnostic = vim.deepcopy(d)
                    mapped_diagnostic.range = local_range(origin, d.range, true)
                    if d.relatedInformation then
                      mapped_diagnostic.relatedInformation = response(s, d.relatedInformation)
                    end
                    table.insert(report.items, mapped_diagnostic)
                  end
                end
                report.relatedDocuments = nil
                return report
              end
              return response(s, result, origin)
            end)
            if ok then result = mapped
            else err, result = { code = -32603, message = tostring(mapped) }, nil end
          end
        end
        callback(err, result, id)
      end, notify_reply)
    end
    sessions[#sessions + 1] = { refresh = refresh, rpc = rpc }
    return proxy
  end
end

-- Completion consumers (Neovim's built-in completion and plugins) apply additional
-- edits through this public utility. Route only explicitly tagged notebook edits; delegate
-- all ordinary buffers and untagged edits unchanged. Validate the entire batch first.
local edit_routing_installed = false
local function install_edit_routing()
  if edit_routing_installed then return end
  edit_routing_installed = true
  local apply_text_edits = vim.lsp.util.apply_text_edits
  vim.lsp.util.apply_text_edits = function(edits, bufnr, encoding)
    local tagged = false
    for _, e in ipairs(edits) do if e._nbv_uri then tagged = true; break end end
    if not tagged then return apply_text_edits(edits, bufnr, encoding) end
    if bufnr == 0 then bufnr = api.nvim_get_current_buf() end
    local groups = {}
    for _, e in ipairs(edits) do
      local dest = e._nbv_uri and vim.uri_to_bufnr(e._nbv_uri) or bufnr
      assert(not e._nbv_uri or vim.b[dest].nbv_key, 'nbv: completion edit targets a removed cell')
      if e._nbv_uri and dest ~= bufnr then
        assert(table.concat(api.nvim_buf_get_lines(dest, 0, -1, false), '\n') == e._nbv_source,
          'nbv: completion edit targets a cell that changed since completion')
      end
      local copy = vim.deepcopy(e)
      copy._nbv_uri, copy._nbv_source = nil, nil
      groups[dest] = groups[dest] or {}
      table.insert(groups[dest], copy)
    end
    for dest, batch in pairs(groups) do apply_text_edits(batch, dest, encoding) end
  end
end

--- Opt into one configured server. Ordinary files retain automatic attachment.
function M.enable(name)
  assert(not server or server == name, 'nbv: configure exactly one notebook language server')
  assert(vim.lsp.config[name], 'nbv: unknown language-server configuration: ' .. name)
  install_edit_routing()
  -- Late enablement migrates only notebook attachments. Ordinary buffers retain the
  -- original configured client, and its automatic activation remains enabled.
  for _, existing in ipairs(vim.lsp.get_clients({ name = name })) do
    for buf in pairs(vim.deepcopy(existing.attached_buffers)) do
      if vim.b[buf].nbv_key then vim.lsp.buf_detach_client(buf, existing.id) end
    end
    if not next(existing.attached_buffers) then existing:stop() end
  end
  server_config = vim.deepcopy(vim.lsp.config[name])
  local original = server_config.root_dir
  vim.lsp.config(name, { root_dir = function(bufnr, on_dir)
    if vim.b[bufnr].nbv_key then return end
    if type(original) == 'function' then return original(bufnr, on_dir) end
    local root = original or vim.fs.root(bufnr, server_config.root_markers or {})
    if root or not server_config.workspace_required then on_dir(root) end
  end })
  server = name
  local group = api.nvim_create_augroup('nbv_lsp_lifecycle', { clear = true })
  api.nvim_create_autocmd('LspAttach', { group = group, callback = function(ev)
    local attached = vim.lsp.get_client_by_id(ev.data.client_id)
    if attached and attached.name == server .. '-nbv' then client_id = attached.id end
  end })
  api.nvim_create_autocmd('LspDetach', { group = group, callback = function(ev)
    vim.schedule(function()
      if not document or not api.nvim_buf_is_valid(ev.buf) then return end
      local detached = vim.lsp.get_client_by_id(ev.data.client_id)
      if not detached or detached:is_stopped() or detached.name ~= server .. '-nbv' then return end
      local key = vim.b[ev.buf].nbv_key
      for _, cell in ipairs(document.cells) do
        if cell.key == key then vim.lsp.buf_attach_client(ev.buf, detached.id); return end
      end
    end)
  end })
  if enabled_callback then enabled_callback() end
end

function M.enabled()
  return server ~= nil
end

function M.on_enable(callback)
  enabled_callback = callback
end

-- Install the cell factory once, independently of the ordered source snapshot. A placement
-- update never rebuilds or refreshes the projection; source changes use on_lines instead.
function M.prepare(create)
  if factory then return end
  factory = create
  if document then M.sync(document) end
end

function M.sync(spec, create)
  if not server then return end
  if create then factory = create end
  local next_membership = { spec.name, spec.language }
  for _, cell in ipairs(spec.cells) do next_membership[#next_membership + 1] = cell.key end
  next_membership = vim.json.encode(next_membership)
  if membership ~= next_membership then
    generation, membership, starting = generation + 1, next_membership, nil
  end
  if spec.language ~= 'python' then
    if client_id then
      local client = vim.lsp.get_client_by_id(client_id)
      if client then client:stop(true) end
    end
    client_id, document = nil, nil
    return
  end
  document = spec
  if not factory then return end
  local by_key = buffers()
  -- Materialize every code cell: navigation and workspace edits can target offscreen cells.
  for _, cell in ipairs(document.cells) do
    if not by_key[cell.key] then by_key[cell.key] = factory(cell) end
    local b = by_key[cell.key]
    if not watched[b] then
      watched[b] = true
      api.nvim_buf_attach(b, false, { on_lines = function()
        schedule_refresh()
      end, on_detach = function() watched[b] = nil end })
    end
  end
  -- Refresh membership once before attaching new cells, so their per-cell didOpen is
  -- already known to the transport and suppressed without another source snapshot.
  for _, session in ipairs(sessions) do
    if not session.rpc.is_closing() then session.refresh() end
  end
  local client = client_id and vim.lsp.get_client_by_id(client_id)
  if not client then
    for _, candidate in ipairs(vim.lsp.get_clients({ name = server .. '-nbv' })) do
      if not candidate:is_stopped() then client, client_id = candidate, candidate.id; break end
    end
  end
  if client and client:is_stopped() then client = nil end
  if not client and not starting and #document.cells > 0 then
    local startup_generation = generation
    starting = startup_generation
    local config = vim.deepcopy(server_config)
    config.name = server .. '-nbv'
    -- Resolve the same configured project root, including asynchronous root_dir callbacks.
    local first_key = document.cells[1].key
    local first_buf = by_key[first_key]
    local function start(root)
      if generation ~= startup_generation or starting ~= startup_generation then return end
      starting = nil
      if config.workspace_required and not root then return end
      if not document or document.language ~= 'python' or document.cells[1].key ~= first_key
        or not api.nvim_buf_is_valid(first_buf) or vim.b[first_buf].nbv_key ~= first_key then return end
      local current_buffers = buffers()
      config.root_dir = root
      config.cmd = transport(vim.deepcopy(config))
      client_id = vim.lsp.start(config, { bufnr = first_buf, reuse_client = function() return false end })
      assert(client_id, 'nbv: could not start notebook language server ' .. server)
      for _, c in ipairs(document.cells) do
        local b = current_buffers[c.key]
        if b and api.nvim_buf_is_valid(b) then vim.lsp.buf_attach_client(b, client_id) end
      end
    end
    if type(config.root_dir) == 'function' then
      config.root_dir(by_key[document.cells[1].key], start)
    else
      local root = config.root_dir or vim.fs.root(document.name, config.root_markers or {})
      start(root)
    end
  elseif client then
    for _, c in ipairs(document.cells) do
      if not vim.lsp.buf_is_attached(by_key[c.key], client.id) then vim.lsp.buf_attach_client(by_key[c.key], client.id) end
    end
  end
end

return M
