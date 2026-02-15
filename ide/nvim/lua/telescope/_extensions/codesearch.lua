-- telescope-codesearch
-- Telescope extension for semantic code search via codesearch
--
-- Usage:
--   require("telescope").load_extension("codesearch")
--   :Telescope codesearch query=<your semantic query>
--
-- Or map it:
--   vim.keymap.set("n", "<leader>cs", function()
--     vim.ui.input({ prompt = "Semantic search: " }, function(query)
--       if query and query ~= "" then
--         require("telescope").extensions.codesearch.codesearch({ query = query })
--       end
--     end)
--   end, { desc = "Semantic code search" })

local pickers = require("telescope.pickers")
local finders = require("telescope.finders")
local conf = require("telescope.config").values
local actions = require("telescope.actions")
local action_state = require("telescope.actions.state")
local entry_display = require("telescope.pickers.entry_display")
local previewers = require("telescope.previewers")

local defaults = {
  bin = "codesearch",
  num = 20,
  min_score = nil,
  language = nil,
  repository = nil,
  data_dir = nil,
  namespace = nil,
}

local function build_cmd(opts)
  local cmd = { opts.bin or defaults.bin }

  if opts.data_dir or defaults.data_dir then
    table.insert(cmd, "--data-dir")
    table.insert(cmd, opts.data_dir or defaults.data_dir)
  end

  if opts.namespace or defaults.namespace then
    table.insert(cmd, "--namespace")
    table.insert(cmd, opts.namespace or defaults.namespace)
  end

  table.insert(cmd, "search")
  table.insert(cmd, opts.query)
  table.insert(cmd, "--format")
  table.insert(cmd, "json")
  table.insert(cmd, "--num")
  table.insert(cmd, tostring(opts.num or defaults.num))

  if opts.min_score or defaults.min_score then
    table.insert(cmd, "--min-score")
    table.insert(cmd, tostring(opts.min_score or defaults.min_score))
  end

  local langs = opts.language or defaults.language
  if langs then
    if type(langs) == "string" then
      langs = { langs }
    end
    for _, l in ipairs(langs) do
      table.insert(cmd, "--language")
      table.insert(cmd, l)
    end
  end

  local repos = opts.repository or defaults.repository
  if repos then
    if type(repos) == "string" then
      repos = { repos }
    end
    for _, r in ipairs(repos) do
      table.insert(cmd, "--repository")
      table.insert(cmd, r)
    end
  end

  return cmd
end

local function parse_results(output)
  local ok, decoded = pcall(vim.json.decode, output)
  if not ok or type(decoded) ~= "table" then
    return {}
  end
  return decoded
end

local displayer = entry_display.create({
  separator = " ",
  items = {
    { width = 6 },
    { width = 12 },
    { remaining = true },
  },
})

local function make_entry(result)
  local symbol = result.symbol_name or result.node_type
  local display = function(entry)
    return displayer({
      { string.format("%.3f", entry.score), "TelescopeResultsNumber" },
      { entry.symbol or "", "TelescopeResultsIdentifier" },
      { entry.filename .. ":" .. entry.lnum, "TelescopeResultsComment" },
    })
  end

  return {
    value = result,
    display = display,
    ordinal = (result.file_path or "") .. " " .. (symbol or "") .. " " .. (result.content or ""),
    filename = result.file_path,
    lnum = result.start_line,
    col = 1,
    score = result.score,
    symbol = symbol,
    node_type = result.node_type,
    content = result.content,
  }
end

local function codesearch(opts)
  opts = vim.tbl_deep_extend("force", defaults, opts or {})

  if not opts.query or opts.query == "" then
    vim.ui.input({ prompt = "Semantic search: " }, function(query)
      if query and query ~= "" then
        opts.query = query
        codesearch(opts)
      end
    end)
    return
  end

  local cmd = build_cmd(opts)
  local result = vim.system(cmd, { text = true }):wait()

  if result.code ~= 0 then
    vim.notify("codesearch failed: " .. (result.stderr or "unknown error"), vim.log.levels.ERROR)
    return
  end

  local results = parse_results(result.stdout)
  if #results == 0 then
    vim.notify("No results found.", vim.log.levels.INFO)
    return
  end

  pickers
    .new(opts, {
      prompt_title = "Codesearch: " .. opts.query,
      finder = finders.new_table({
        results = results,
        entry_maker = make_entry,
      }),
      sorter = conf.generic_sorter(opts),
      previewer = previewers.new_buffer_previewer({
        title = "Code Preview",
        define_preview = function(self, entry)
          -- Use Telescope's built-in file preview at the right line
          conf.buffer_previewer_maker(entry.filename, self.state.bufnr, {
            bufname = entry.filename,
            callback = function(bufnr)
              -- Jump to the start line in the preview
              pcall(vim.api.nvim_buf_call, bufnr, function()
                pcall(vim.cmd, "normal! " .. entry.lnum .. "Gzz")
              end)
            end,
          })
        end,
      }),
      attach_mappings = function(prompt_bufnr, map)
        actions.select_default:replace(function()
          actions.close(prompt_bufnr)
          local entry = action_state.get_selected_entry()
          if entry then
            vim.cmd("edit " .. vim.fn.fnameescape(entry.filename))
            vim.api.nvim_win_set_cursor(0, { entry.lnum, 0 })
            vim.cmd("normal! zz")
          end
        end)
        return true
      end,
    })
    :find()
end

return require("telescope").register_extension({
  setup = function(ext_config)
    defaults = vim.tbl_deep_extend("force", defaults, ext_config or {})
  end,
  exports = {
    codesearch = codesearch,
  },
})
