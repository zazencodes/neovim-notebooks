//! Real Pyright through the public Neovim client, with ordinary editable cell buffers.
use nbv_core::{CellKind, Change};
use nbv_nvim::harness::{Harness, Options};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

async fn value(h: &Harness, code: &str) -> Value {
    let v = h.lua(&format!("return vim.json.encode((function() {code} end)())")).await;
    serde_json::from_str(v.as_str().unwrap()).unwrap()
}

async fn until(h: &Harness, code: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let v = value(h, code).await;
        if v != Value::Null && v != false {
            return v;
        }
        assert!(Instant::now() < deadline, "LSP timeout: {code}\n{}", h.screen());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn open(encoding: &str) -> Option<Harness> {
    let Ok(server) = std::env::var("NBV_LSP_SERVER") else {
        assert!(std::env::var_os("CI").is_none(), "CI must provision NBV_LSP_SERVER");
        eprintln!("skipping real Pyright test; set NBV_LSP_SERVER to pyright-langserver");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let notebook = dir.path().canonicalize().unwrap().join("context.ipynb");
    std::fs::write(
        &notebook,
        serde_json::to_vec(&json!({
        "nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[
            {"id":"first","cell_type":"code","metadata":{},"outputs":[],"execution_count":null,
             "source":"shared_value = 42\nlabel = '😀'; café = shared_value"},
            {"id":"second","cell_type":"code","metadata":{},"outputs":[],"execution_count":null,
             "source":"result = shared_value\nlabel = '😀'; answer = café\nbroken = missing_name"}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let command = if encoding == "utf-8" {
        format!(
            "assert(loadstring({}))()({})",
            serde_json::to_string(include_str!("pyright_utf8.lua")).unwrap(),
            serde_json::to_string(&server).unwrap()
        )
    } else {
        format!("{{ {}, '--stdio' }}", serde_json::to_string(&server).unwrap())
    };
    let init = format!(
        r#"
        local base_command = {}
        local command = function(d, config)
          local rpc = type(base_command) == 'function' and base_command(d, config)
            or vim.lsp.rpc.start(base_command, d)
          if config.name == 'test_pyright-nbv' then
            _G.notebook_dispatchers = d
            _G.wire = {{ opened = {{}} }}
            local request = rpc.request
            rpc.request = function(method, params, callback, notify_reply)
              if method == 'textDocument/completion' and _G.native_completion_probe then
                return request(method, params, function(err,result,id)
                  -- Pyright may cache eager import edits after earlier completion requests.
                  -- Defer those real edits to resolve to exercise CompleteDone deterministically.
                  _G.deferred_imports = {{}}
                  for _,item in ipairs(result.items or result) do
                    if item.additionalTextEdits then
                      deferred_imports[vim.json.encode(item.data)] = item.additionalTextEdits
                      item.additionalTextEdits = nil
                    end
                  end
                  callback(err,result,id)
                end, notify_reply)
              end
              if method == 'completionItem/resolve' and _G.native_completion_probe then
                _G.native_resolve_source = table.concat(vim.api.nvim_buf_get_lines(bufs.second,0,-1,false), '\n')
                return request(method, params, function(err,result,id)
                  if result and not result.additionalTextEdits then
                    result.additionalTextEdits = deferred_imports[vim.json.encode(params.data)]
                  end
                  callback(err,result,id)
                end, notify_reply)
              end
              if method == 'textDocument/definition' and _G.external_link_probe then
                _G.external_link_probe = nil
                return request(method, params, function(err,result,id)
                  callback(nil, {{{{targetUri='file:///external/library.py',
                    originSelectionRange={{start={{line=params.position.line,character=0}},['end']={{line=params.position.line,character=1}}}},
                    targetRange={{start={{line=0,character=0}},['end']={{line=1,character=0}}}},
                    targetSelectionRange={{start={{line=0,character=0}},['end']={{line=0,character=4}}}}}}}}, id)
                end, notify_reply)
              end
              if method == 'textDocument/definition' and _G.external_probe then
                _G.external_probe = nil
                return request(method, params, function(err,result,id)
                  _G.external_original = vim.deepcopy(result)
                  callback(err,result,id)
                end, notify_reply)
              end
              if method == 'textDocument/completion' and _G.default_completion_probe then
                local variant = _G.default_completion_probe
                _G.default_completion_probe = nil
                return request(method, params, function(err, result, id)
                  local range = {{start={{line=params.position.line,character=9}},['end']={{line=params.position.line,character=17}}}}
                  local edit_range = variant == 'insertReplace' and {{insert=range,replace=range}} or range
                  callback(nil, {{itemDefaults={{editRange=edit_range}},items={{{{label='defaults'}}}}}}, id)
                end, notify_reply)
              end
              return request(method, params, callback, notify_reply)
            end
            local notify = rpc.notify
            rpc.notify = function(method, params)
              if method == 'textDocument/didOpen' then
                wire.uri = params.textDocument.uri
                wire.version = params.textDocument.version
                table.insert(wire.opened, params.textDocument.uri)
              elseif method == 'textDocument/didChange' then
                wire.version = params.textDocument.version
              end
              return notify(method, params)
            end
          end
          return rpc
        end
        vim.lsp.config('test_pyright', {{ cmd = command, filetypes = {{'python'}},
          settings = {{ python = {{ analysis = {{ diagnosticMode = 'openFilesOnly', autoImportCompletions = true }} }} }} }})
        vim.lsp.enable('test_pyright')
        require('nbv.lsp').enable('test_pyright')
    "#,
        command
    );
    let h = Harness::open_path(dir, notebook, Options { init: Some(init), ..Options::default() }).await;
    h.layout_all(None).await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients()) do if c.name == 'test_pyright-nbv' and c.initialized then _G.client = c; return true end end return false").await;
    h.lua(
        r#"
      _G.bufs = {}
      for _,b in ipairs(vim.api.nvim_list_bufs()) do
        if vim.b[b].nbv_key then bufs[vim.b[b].nbv_key] = b end
      end
      _G.request = function(method, params, b)
        params.textDocument = {uri=vim.uri_from_bufnr(b)}
        local r = client:request_sync(method, params, 8000, b)
        assert(r, 'server request timed out')
        assert(not r.err, vim.inspect(r.err))
        return r.result
      end
    "#,
    )
    .await;
    Some(h)
}

#[tokio::test]
async fn pyright_cross_cell_context_edits_and_lifecycle() {
    for encoding in ["utf-16", "utf-8"] {
        exercise_pyright(encoding).await;
    }
}

async fn exercise_pyright(encoding: &str) {
    let Some(h) = open(encoding).await else { return };
    h.lua(&format!("_G.character = {}", if encoding == "utf-8" { 26 } else { 24 })).await;
    // One shared client, no accidental per-cell automatic Pyright clients.
    assert_eq!(value(&h, "return #vim.lsp.get_clients()").await, 1);
    assert_eq!(value(&h, "return #wire.opened").await, 1);
    assert!(value(&h, "return wire.opened[1]").await.as_str().unwrap().ends_with(".nvb.py"));
    // An asynchronous stale push must never be published on the newly mapped cells.
    h.lua(
        r#"notebook_dispatchers.notification('textDocument/publishDiagnostics', {
      uri=wire.uri, version=wire.version-1, diagnostics={{
      range={start={line=3,character=0},['end']={line=3,character=1}},
      message='stale_probe',severity=1}}})"#,
    )
    .await;
    assert!(!value(&h, "return vim.diagnostic.get()").await.to_string().contains("stale_probe"));
    let diagnostics =
        until(&h, "local d = vim.diagnostic.get(bufs.second); if #d > 0 then return d end return false").await;
    assert!(diagnostics.as_array().unwrap().iter().any(|d| d["message"].as_str().unwrap().contains("missing_name")));
    assert!(!diagnostics.to_string().contains("shared_value\" is not defined"));
    let definition =
        value(&h, "return request('textDocument/definition', {position={line=0,character=12}}, bufs.second)").await;
    assert!(definition[0]["uri"].as_str().unwrap().contains("first.py"), "{definition}");
    assert_eq!(definition[0]["range"]["start"]["line"], 0);
    let refs = value(&h, "return request('textDocument/references', {position={line=0,character=12}, context={includeDeclaration=true}}, bufs.second)").await;
    assert!(refs.as_array().unwrap().iter().any(|r| r["uri"].as_str().unwrap().contains("first.py")));
    assert!(refs.as_array().unwrap().iter().any(|r| r["uri"].as_str().unwrap().contains("second.py")));
    let completions =
        value(&h, "return request('textDocument/completion', {position={line=0,character=17}}, bufs.second)").await;
    let items = completions.get("items").unwrap_or(&completions).as_array().unwrap();
    assert!(items.iter().any(|i| i["label"] == "shared_value"));
    for variant in ["range", "insertReplace"] {
        h.lua(&format!("_G.default_completion_probe = '{}'", variant)).await;
        let defaults =
            value(&h, "return request('textDocument/completion', {position={line=0,character=17}}, bufs.second)").await;
        let edit_range = &defaults["itemDefaults"]["editRange"];
        if variant == "range" {
            assert_eq!(edit_range["start"]["line"], 0, "{defaults}");
        } else {
            assert_eq!(edit_range["insert"]["start"]["line"], 0, "{defaults}");
            assert_eq!(edit_range["replace"]["start"]["line"], 0, "{defaults}");
        }
    }
    let prepared =
        value(&h, "return request('textDocument/prepareRename', {position={line=1,character=character}}, bufs.second)")
            .await;
    let prepared_range = prepared.get("range").unwrap_or(&prepared);
    assert_eq!(prepared_range["start"]["line"], 1, "{prepared}");
    // Positions following an astral character and non-ASCII identifier. Native Pyright
    // uses UTF-16; the test bridge exercises UTF-8 on the Neovim side against real Pyright.
    assert_eq!(value(&h, "return client.offset_encoding").await, encoding);
    let rename = value(
        &h,
        "return request('textDocument/rename', {position={line=1,character=character},newName='renamed_café'}, bufs.second)",
    )
    .await;
    assert!(rename.to_string().contains("first.py"), "{rename}");
    assert!(rename.to_string().contains("second.py"), "{rename}");
    h.lua("local e = request('textDocument/rename', {position={line=1,character=character},newName='renamed_café'}, bufs.second); vim.lsp.util.apply_workspace_edit(e, client.offset_encoding)").await;
    until(&h, "return vim.api.nvim_buf_get_lines(bufs.first,1,2,false)[1]:find('renamed_café',1,true) ~= nil").await;
    assert!(
        value(&h, "return vim.api.nvim_buf_get_lines(bufs.second,1,2,false)[1]")
            .await
            .as_str()
            .unwrap()
            .contains("renamed_café")
    );
    // Unsaved edits reach the server without saving the notebook.
    h.lua("vim.api.nvim_buf_set_lines(bufs.first,0,1,false,{'unsaved_symbol = 7'}); vim.api.nvim_buf_set_lines(bufs.second,0,1,false,{'result = unsaved_symbol'})").await;
    let definition =
        value(&h, "return request('textDocument/definition', {position={line=0,character=12}}, bufs.second)").await;
    assert!(definition[0]["uri"].as_str().unwrap().contains("first.py"));
    assert!(!std::fs::read_to_string(&h.notebook).unwrap().contains("unsaved_symbol"));
    // Resolve a real auto-import completion in cell 2. Its import belongs in cell 1.
    h.lua("vim.api.nvim_buf_set_lines(bufs.second, -1, -1, false, {'factory = Coun'})").await;
    let auto_import = value(
        &h,
        r#"
      local all = request('textDocument/completion', {position={line=3,character=14}}, bufs.second)
      for _, item in ipairs(all.items or all) do
        if item.label == 'Counter' then
          local resolved = client:request_sync('completionItem/resolve', item, 8000, bufs.second)
          assert(resolved and not resolved.err, vim.inspect(resolved))
          if resolved.result.additionalTextEdits and #resolved.result.additionalTextEdits > 0 then
            _G.import_item = resolved.result
            return import_item
          end
        end
      end
      error('Pyright did not offer a Counter auto-import')
    "#,
    )
    .await;
    assert!(auto_import["additionalTextEdits"][0]["_nbv_uri"].as_str().unwrap().contains("first.py"), "{auto_import}");
    h.lua("vim.lsp.util.apply_text_edits(import_item.additionalTextEdits, bufs.second, client.offset_encoding)").await;
    assert!(
        value(&h, "return table.concat(vim.api.nvim_buf_get_lines(bufs.first,0,-1,false),'\\n')")
            .await
            .as_str()
            .unwrap()
            .contains("import Counter")
    );
    assert!(
        !value(&h, "return table.concat(vim.api.nvim_buf_get_lines(bufs.second,0,-1,false),'\\n')")
            .await
            .as_str()
            .unwrap()
            .contains("import Counter")
    );
    assert_eq!(auto_import["textEdit"]["range"]["start"]["line"], 3, "{auto_import}");
    h.lua("vim.lsp.util.apply_text_edits({import_item.textEdit}, bufs.second, client.offset_encoding)").await;
    assert_eq!(value(&h, "return vim.api.nvim_buf_get_lines(bufs.second,3,4,false)[1]").await, "factory = Counter");
    h.lua("_G.external_probe = true").await;
    let external =
        value(&h, "return request('textDocument/definition', {position={line=3,character=12}}, bufs.second)").await;
    assert_eq!(external, value(&h, "return external_original").await);
    assert!(!external.to_string().contains("context.ipynb"), "{external}");
    h.lua("_G.external_link_probe = true").await;
    let link =
        value(&h, "return request('textDocument/definition', {position={line=3,character=12}}, bufs.second)").await;
    assert_eq!(link[0]["targetRange"]["start"]["line"], 0);
    assert_eq!(link[0]["targetSelectionRange"]["start"]["line"], 0);
    assert_eq!(link[0]["originSelectionRange"]["start"]["line"], 3);
    // Server-initiated versioned documentChanges split into two real cell edits.
    let applied = value(&h, r#"
      local first_count = vim.api.nvim_buf_line_count(bufs.first)
      return notebook_dispatchers.server_request('workspace/applyEdit', {edit={documentChanges={{
        textDocument={uri=wire.uri,version=wire.version},edits={
          {range={start={line=first_count-2,character=17},['end']={line=first_count-2,character=18}},newText='9'},
          {range={start={line=first_count+4,character=17},['end']={line=first_count+4,character=17}},newText='  # mapped'}
        }}}}})
    "#).await;
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(
        value(&h, "return vim.api.nvim_buf_get_lines(bufs.second,3,4,false)[1]").await,
        "factory = Counter  # mapped"
    );
    let rejected = value(
        &h,
        r#"
      local line = vim.api.nvim_buf_line_count(bufs.first)
      return notebook_dispatchers.server_request('workspace/applyEdit', {edit={changes={
        [wire.uri]={{range={start={line=line,character=0},['end']={line=line,character=0}},newText='bad'}}
      }}})
    "#,
    )
    .await;
    assert_eq!(rejected["applied"], false, "{rejected}");
    assert!(rejected["failureReason"].as_str().unwrap().contains("synthetic"));
    // Accept a completion through Neovim's native popup. CompleteDone inserts Counter
    // before requesting deferred resolve; the auto-import must still reach cell 1.
    h.lua("vim.api.nvim_buf_set_lines(bufs.first,0,1,false,{}); vim.api.nvim_buf_set_lines(bufs.second,-1,-1,false,{'queue = Coun'})").await;
    h.enter(&h.key(1), false).await;
    h.lua("vim.lsp.completion.enable(true,client.id,bufs.second,{autotrigger=false}); vim.opt.completeopt={'menu','menuone','noselect'}; vim.api.nvim_win_set_cursor(0,{5,11}); _G.native_completion_probe=true; _G.native_resolve_source=nil").await;
    h.keys("a").await;
    h.lua("vim.lsp.completion.get()").await;
    let choice = until(&h, "if vim.fn.pumvisible() ~= 1 then return false end; for i,item in ipairs(vim.fn.complete_info({'items'}).items) do if item.word == 'Counter' then return i end end return false").await.as_u64().unwrap();
    h.keys(&format!("{}<C-y>", "<C-n>".repeat(choice as usize))).await;
    until(&h, "return table.concat(vim.api.nvim_buf_get_lines(bufs.first,0,-1,false),'\\n'):find('import Counter',1,true) ~= nil").await;
    assert!(value(&h, "return native_resolve_source").await.as_str().unwrap().contains("queue = Counter"));
    assert_eq!(value(&h, "return vim.api.nvim_buf_get_lines(bufs.second,4,5,false)[1]").await, "queue = Counter");
    h.keys("<Esc>").await;
    h.lua("_G.native_completion_probe=false").await;
    let stale =
        value(&h, "local r = client:request_sync('completionItem/resolve',import_item,8000,bufs.second); return r.err")
            .await;
    assert_eq!(stale["code"], -32801, "unrelated edits must invalidate old completion: {stale}");
    // Ordinary files still use the user's automatically enabled server.
    h.cmd("edit ordinary.py").await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients()) do if c.name == 'test_pyright' and c.initialized then return true end end return false").await;
    // A detached cell is reattached on synchronization; native :lsp restart retains
    // the notebook transport and user callbacks without opening fragment documents.
    h.lua("vim.lsp.buf_detach_client(bufs.second, client.id)").await;
    h.layout_all(None).await;
    assert_eq!(value(&h, "return vim.lsp.buf_is_attached(bufs.second,client.id)").await, true);
    h.enter(&h.key(1), false).await;
    h.lua("_G.previous_client = client.id").await;
    h.cmd("lsp restart").await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients()) do if c.name == 'test_pyright-nbv' and c.id ~= previous_client and c.initialized then _G.client=c; return true end end return false").await;
    let d = value(&h, "return request('textDocument/definition', {position={line=0,character=12}}, bufs.second)").await;
    assert!(d.to_string().contains("first.py"), "definition after restart: {d}");
    // Reordering, kind changes and deletion update the same shadow document.
    h.lua("_G.before_move = wire.version").await;
    {
        let mut s = h.state.lock().unwrap();
        let first = s.nb.order()[0].clone();
        s.nb.edit(vec![Change::Move { key: first, index: 1 }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
        editor.sync(nb);
    }
    h.settle().await;
    h.lua(
        r#"notebook_dispatchers.notification('textDocument/publishDiagnostics', {
      uri=wire.uri, version=before_move, diagnostics={{
      range={start={line=0,character=0},['end']={line=0,character=1}},
      message='stale_after_move',severity=1}}})"#,
    )
    .await;
    assert!(!value(&h, "return vim.diagnostic.get()").await.to_string().contains("stale_after_move"));
    // The line in cell 1 is still addressed locally after the order changes.
    let d = value(&h, "return request('textDocument/definition', {position={line=0,character=12}}, bufs.second)").await;
    // Notebook order is textual order: Pyright correctly refuses a value used before
    // its declaration after this move. Restore the order and verify mapped navigation.
    assert!(d.is_null(), "definition before declaration: {d}");
    {
        let mut s = h.state.lock().unwrap();
        s.nb.undo();
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
        editor.sync(nb);
    }
    h.settle().await;
    let d = value(&h, "return request('textDocument/definition', {position={line=0,character=12}}, bufs.second)").await;
    assert!(d.to_string().contains("first.py"), "{d}");
    {
        let mut s = h.state.lock().unwrap();
        let first = s.nb.order()[0].clone();
        s.nb.edit(vec![Change::Kind { key: first, kind: CellKind::Markdown }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
        editor.sync(nb);
    }
    h.settle().await;
    until(&h, "for _,d in ipairs(vim.diagnostic.get(bufs.second)) do if d.message:find('unsaved_symbol',1,true) then return true end end return false").await;

    let inserted = {
        let mut s = h.state.lock().unwrap();
        let key = s.nb.new_cell(CellKind::Code, "inserted_symbol = 9");
        s.nb.edit(vec![Change::Show { key: key.clone(), index: 0 }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
        editor.sync(nb);
        key
    };
    h.settle().await;
    h.lua("vim.api.nvim_buf_set_lines(bufs.second,0,1,false,{'result = inserted_symbol'})").await;
    let d = value(&h, "return request('textDocument/definition', {position={line=0,character=12}}, bufs.second)").await;
    assert!(d.to_string().contains(inserted.as_str()), "definition in new offscreen cell: {d}");
    {
        let mut s = h.state.lock().unwrap();
        s.nb.edit(vec![Change::Hide { key: inserted }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
        editor.sync(nb);
    }
    h.settle().await;
    until(&h, "for _,d in ipairs(vim.diagnostic.get(bufs.second)) do if d.message:find('inserted_symbol',1,true) then return true end end return false").await;
    h.state.lock().unwrap().editor.set_language(Some("julia".into()));
    h.layout_all(None).await;
    until(&h, "return #vim.lsp.get_clients({name='test_pyright-nbv'}) == 0").await;
    // The ordinary-file client survives a notebook kernel-language switch.
    assert_eq!(value(&h, "return #vim.lsp.get_clients({name='test_pyright'})").await, 1);
    h.state.lock().unwrap().editor.set_language(Some("python".into()));
    h.layout_all(None).await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients()) do if c.name == 'test_pyright-nbv' and c.initialized then _G.client=c; return true end end return false").await;
}

#[tokio::test]
async fn deferred_root_callback_cannot_start_for_replaced_cells() {
    let h = Harness::open(
        "v4.5-outputs.ipynb",
        Options {
            init: Some(
                r#"
          _G.roots = {}
          vim.lsp.config('deferred', {cmd={'must-never-be-executed'},filetypes={'python'},
            root_dir=function(buf, callback) table.insert(roots,{buf=buf,callback=callback}) end})
          require('nbv.lsp').enable('deferred')
        "#
                .into(),
            ),
            ..Options::default()
        },
    )
    .await;
    h.layout_all(None).await;
    assert_eq!(value(&h, "return #roots").await, 1);
    // Change the first code cell to markdown while root discovery is in flight.
    {
        let mut s = h.state.lock().unwrap();
        let code = s.nb.order().iter().find(|k| s.nb.cell(k).unwrap().kind() == CellKind::Code).unwrap().clone();
        s.nb.edit(vec![Change::Kind { key: code, kind: CellKind::Markdown }]);
        let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
        editor.sync(nb);
    }
    h.settle().await;
    // A stale callback cannot launch a process or attach the replacement cell.
    h.lua("roots[1].callback(vim.fn.getcwd())").await;
    assert_eq!(value(&h, "return #vim.lsp.get_clients()").await, 0);
    h.state.lock().unwrap().editor.set_language(Some("julia".into()));
    h.layout_all(None).await;
    h.lua("for _,root in ipairs(roots) do root.callback(vim.fn.getcwd()) end").await;
    assert_eq!(value(&h, "return #vim.lsp.get_clients()").await, 0);
}

#[tokio::test]
async fn notebook_lsp_preserves_filetype_windows_and_required_roots() {
    for enabled in [false, true] {
        let init = format!(
            r#"
          vim.api.nvim_create_autocmd('FileType', {{pattern='python',callback=function() vim.wo.colorcolumn='73' end}})
          vim.lsp.config('required', {{cmd={{'must-never-be-executed'}},filetypes={{'python'}},
            root_markers={{'this-marker-does-not-exist-nbv-test'}},workspace_required=true}})
          vim.lsp.enable('required')
          {}
        "#,
            if enabled { "require('nbv.lsp').enable('required')" } else { "" }
        );
        let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init), ..Options::default() }).await;
        h.layout_all(None).await;
        let settings = value(
            &h,
            r#"
          local columns = {}
          for _,w in ipairs(vim.api.nvim_list_wins()) do
            if vim.w[w].nbv_key and vim.bo[vim.api.nvim_win_get_buf(w)].filetype == 'python' then
              table.insert(columns, vim.wo[w].colorcolumn)
            end
          end
          return columns
        "#,
        )
        .await;
        assert!(!settings.as_array().unwrap().is_empty());
        assert!(settings.as_array().unwrap().iter().all(|c| c == "73"), "LSP enabled={enabled}: {settings}");
        assert_eq!(value(&h, "return #vim.lsp.get_clients()").await, 0, "required root is absent");
    }
}

#[tokio::test]
async fn placements_never_reconstruct_or_transmit_notebook_sources() {
    for enabled in [false, true] {
        let init = format!(
            r#"
          vim.lsp.config('deferred', {{cmd={{'must-never-be-executed'}},filetypes={{'python'}},
            root_dir=function() end}})
          {}
          _G.source_specs, _G.placements = 0, 0
          local lsp = require('nbv.lsp')
          local sync = lsp.sync
          lsp.sync = function(spec, create)
            if spec then source_specs = source_specs + 1 end
            return sync(spec, create)
          end
          local layout = require('nbv').layout
          require('nbv').layout = function(seq, spec)
            assert(spec.notebook == nil, 'placement contains full notebook sources')
            placements = placements + 1
            return layout(seq, spec)
          end
        "#,
            if enabled { "require('nbv.lsp').enable('deferred')" } else { "" }
        );
        let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init), ..Options::default() }).await;
        let mut rects = h.layout_all(None).await;
        let initial = h.state.lock().unwrap().editor.lsp_context_stats();
        assert_eq!(initial.documents, usize::from(enabled));
        assert_eq!(initial.source_bytes > 0, enabled);
        let received = value(&h, "return source_specs").await;
        for i in 0..12 {
            for rect in &mut rects {
                rect.col = 4 + i % 2;
            }
            h.layout(None, &rects).await;
        }
        assert_eq!(
            h.state.lock().unwrap().editor.lsp_context_stats(),
            initial,
            "scroll placements reconstructed notebook sources; enabled={enabled}"
        );
        assert_eq!(
            value(&h, "return source_specs").await,
            received,
            "scroll placements sent or refreshed full document; enabled={enabled}"
        );
        assert_eq!(value(&h, "return placements").await, 13);
        if !enabled {
            {
                let mut s = h.state.lock().unwrap();
                let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
                editor.sync(nb);
            }
            h.settle().await;
            assert_eq!(
                h.state.lock().unwrap().editor.lsp_context_stats(),
                initial,
                "disabled structural synchronization must not reconstruct LSP sources"
            );
            // Enable after Ready: the existing layout factory receives the initial snapshot.
            h.lua("require('nbv.lsp').enable('deferred')").await;
            h.wait("late notebook LSP enable", |s| s.editor.lsp_context_stats().documents == 1).await;
            h.settle().await;
            assert_eq!(value(&h, "return source_specs").await, 1);
        }
        let before_edit = h.state.lock().unwrap().editor.lsp_context_stats();
        h.lua("for _,b in ipairs(vim.api.nvim_list_bufs()) do if vim.b[b].nbv_key and vim.bo[b].filetype == 'python' then vim.api.nvim_buf_set_lines(b,0,1,false,{'unsaved_placement_probe = 1'}); break end end").await;
        h.layout(None, &rects).await;
        assert_eq!(
            h.state.lock().unwrap().editor.lsp_context_stats(),
            before_edit,
            "live text should use Lua buffer overlay without a Rust document payload"
        );
        {
            let mut s = h.state.lock().unwrap();
            let key = s.nb.new_cell(CellKind::Code, "topology_probe = 2");
            s.nb.edit(vec![Change::Show { key, index: 0 }]);
            let nbv_nvim::harness::State { nb, editor, .. } = &mut *s;
            editor.sync(nb);
        }
        h.settle().await;
        assert_eq!(h.state.lock().unwrap().editor.lsp_context_stats().documents, before_edit.documents + 1);
        assert_eq!(value(&h, "local count = 0; for _,b in ipairs(vim.api.nvim_list_bufs()) do if vim.b[b].nbv_key and vim.api.nvim_buf_get_lines(b,0,1,false)[1] == 'topology_probe = 2' then count=count+1 end end; return count").await, 1,
            "topology sync must materialize offscreen cells through the retained factory");
    }
}

#[tokio::test]
async fn late_enable_migrates_only_notebook_attachments() {
    let Ok(server) = std::env::var("NBV_LSP_SERVER") else {
        assert!(std::env::var_os("CI").is_none(), "CI must provision NBV_LSP_SERVER");
        return;
    };
    let init = format!(
        r#"
      vim.lsp.config('late_pyright', {{cmd={{{},'--stdio'}},filetypes={{'python'}}}})
      vim.lsp.enable('late_pyright')
    "#,
        serde_json::to_string(&server).unwrap()
    );
    let h = Harness::open("v4.5-outputs.ipynb", Options { init: Some(init), ..Options::default() }).await;
    h.layout_all(None).await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients({name='late_pyright'})) do if c.initialized then return true end end return false").await;
    assert_eq!(h.state.lock().unwrap().editor.lsp_context_stats().documents, 0);
    h.cmd("edit ordinary.py").await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients({bufnr=vim.api.nvim_get_current_buf(),name='late_pyright'})) do if c.initialized then _G.ordinary_client=c.id; return true end end return false").await;
    h.lua("require('nbv.lsp').enable('late_pyright')").await;
    until(&h, "for _,c in ipairs(vim.lsp.get_clients({name='late_pyright-nbv'})) do if c.initialized then return true end end return false").await;
    assert_eq!(h.state.lock().unwrap().editor.lsp_context_stats().documents, 1);
    assert_eq!(
        value(&h, "return vim.lsp.buf_is_attached(vim.api.nvim_get_current_buf(), ordinary_client)").await,
        true
    );
    assert_eq!(
        value(
            &h,
            r#"
      for _,b in ipairs(vim.api.nvim_list_bufs()) do
        if vim.b[b].nbv_key and vim.bo[b].filetype == 'python' then
          if #vim.lsp.get_clients({bufnr=b,name='late_pyright'}) ~= 0 then return false end
          if #vim.lsp.get_clients({bufnr=b,name='late_pyright-nbv'}) ~= 1 then return false end
        end
      end
      return true
    "#
        )
        .await,
        true
    );
}
