# Changelog

## [0.3.0]

### Added
- Comprehensive tool support with mode selection via `--tools ro|rw`
- New read tools: `list_files`, `find`, `grep_regex`, `grep_exact`, and `read_file`
- New write/edit tools: `write_file`, `edit_file`, and `replace_lines`
- `--hidden` flag to explicitly allow tool access to hidden files/directories (dotpaths)
- `--verbose` flag to show model-requested tool calls during execution
- Strict tool argument validation using `#[serde(deny_unknown_fields)]` for all tool parameter schemas
- Path policy enforcement utilities to keep tool access scoped to the workspace root
- Streaming markdown table rendering with aligned output
- Expanded markdown support in streaming mode (lists, blockquotes, links, horizontal rules)
- Line addition/removal stats in file diffs and approval prompts
- Input-line syntax highlighting in readline using configured prompt color
- Default model alias `codex` mapped to `openai/gpt-5.3-codex`

### Changed
- Default model changed from `sonnet` to `codex` (including config defaults and docs/examples)
- Tool architecture refactored into explicit `ToolMode` (`Disabled`, `ReadOnly`, `ReadWrite`)
- System prompt generation now adapts to tool mode, output file permissions, and hidden-path policy
- File operation flow refactored to authorize paths through workspace resolution and policy checks
- `save_file` tool renamed/replaced by `write_file` with clearer semantics
- File editing now supports exact single-match replacements and line-range replacement operations
- Overwrite confirmations now show `+/-` line counts and include them in auto-approve output
- Markdown heading rendering restyled with level-specific formatting (including H1 rule line and deep-heading prefixes)
- Inline color parsing centralized in `InlineColors` and reused across renderer/config/readline
- CLI help/examples updated to reflect tool-enabled workflows

### Improved
- Better code fence detection in streaming (marker type/length aware, indentation-aware)
- Better handling of mixed CRLF/LF scenarios when replacing lines
- More robust system prompt updates when output file permissions change during chat
- Safer and cleaner tool call logging with optional truncation/full-args behavior

## [0.2.0]

### Added
- Version flag now displays the package version from Cargo.toml
- Unified streaming architecture that handles both regular responses and tool calls
- Multi-round tool call support (up to 32 rounds) to prevent infinite loops
- Tool-aware streaming using `stream_chat_completion_tool_aware` from openrouter-rs

### Changed
- Updated `openrouter-rs` dependency from 0.4.7 to 0.5.0
- Project name in README now shows "zo - Zettabyte Oracle" instead of just "ZO"
- Refactored response handling to use a single `stream_response()` method instead of separate `stream_and_collect()` and `send_with_tools()` methods
- Tool calls now work with streaming enabled (no more batch-only mode for file operations)
- Improved tool call handling with proper content placeholders for empty messages

### Removed
- Batch mode for tool calls (now everything uses streaming)
