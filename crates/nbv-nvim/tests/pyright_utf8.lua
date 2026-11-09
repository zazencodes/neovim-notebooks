-- Test-only protocol bridge: Pyright supports UTF-16. Expose UTF-8 to Neovim while
-- translating positions at the boundary to the real Pyright process. This exercises
-- the notebook transport and Neovim's edit application in both encodings against the
-- same real server; it does not claim that Pyright natively negotiates UTF-8.
return function(server)
  return function(dispatchers)
    local documents, completion_uri = {}, nil
    local function convert(value, uri, to_utf8)
      if type(value) ~= 'table' or value == vim.NIL then return value end
      local result = vim.deepcopy(value)
      uri = value.uri or value.targetUri or (value.textDocument and value.textDocument.uri) or uri
      if type(value.line) == 'number' and type(value.character) == 'number' then
        if not documents[uri] then
          documents[uri] = vim.fn.readfile(vim.uri_to_fname(uri))
        end
        local lines = documents[uri]
        local line = lines[value.line + 1] or ''
        if to_utf8 then
          result.character = vim.str_byteindex(line, 'utf-16', value.character, false)
        else
          result.character = vim.str_utfindex(line, 'utf-16', value.character, false)
        end
        return result
      end
      for k, v in pairs(value) do
        if k ~= 'data' then
          if k == 'changes' then
            result[k] = {}
            for target, edits in pairs(v) do result[k][target] = convert(edits, target, to_utf8) end
          else result[k] = convert(v, uri, to_utf8) end
        end
      end
      return result
    end
    local wrapped = vim.tbl_extend('force', dispatchers, {
      notification = function(method, params)
        dispatchers.notification(method, convert(params, params.uri, true))
      end,
      server_request = function(method, params)
        return dispatchers.server_request(method, convert(params, nil, true))
      end,
    })
    local rpc = vim.lsp.rpc.start({ server, '--stdio' }, wrapped)
    return {
      is_closing = rpc.is_closing,
      terminate = rpc.terminate,
      notify = function(method, params)
        if method == 'textDocument/didOpen' then
          documents[params.textDocument.uri] = vim.split(params.textDocument.text, '\n', { plain = true })
        elseif method == 'textDocument/didChange' then
          assert(#params.contentChanges == 1 and not params.contentChanges[1].range,
            'test bridge requires full-document changes')
          documents[params.textDocument.uri] = vim.split(params.contentChanges[1].text, '\n', { plain = true })
        end
        return rpc.notify(method, params)
      end,
      request = function(method, params, callback, notify_reply)
        local uri = params and params.textDocument and params.textDocument.uri
        if method == 'textDocument/completion' then completion_uri = uri end
        if method == 'completionItem/resolve' then uri = completion_uri end
        local mapped = convert(params, uri, false)
        if method == 'initialize' then
          mapped.capabilities.general.positionEncodings = { 'utf-16' }
        end
        return rpc.request(method, mapped, function(err, result, id)
          if method == 'initialize' and result then
            result.capabilities.positionEncoding = 'utf-8'
          else result = convert(result, uri, true) end
          callback(err, result, id)
        end, notify_reply)
      end,
    }
  end
end
