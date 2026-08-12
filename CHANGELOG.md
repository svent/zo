# Changelog

## [0.6.0]

### Added
- Configurable reasoning effort for text and chat requests via global `reasoning_effort`, per-model `custom_models[].reasoning_effort`, and the `--reasoning-effort` CLI override
- Configurable byte-based input and session limits through `[limits]`, `--max-input-bytes`, and `--max-session-bytes`
- File-based shell policy DSL under `~/.config/zo/policies/`, with `allow`, `ask`, and `deny` rules; exact and regex argument matching; and `+`, `*`, `++`, and `**` argument cardinality patterns
- Inline `#TEST` directives for validating shell policy behavior when policies are loaded
- Built-in `sol`, `terra`, and `luna` aliases for the GPT-5.6 model family

### Changed
- Default text model changed from `codex` (`openai/gpt-5.4`) to `sol` (`openai/gpt-5.6-sol`)
- Shell policies now live in flat files under `~/.config/zo/policies/` instead of `shell.always_on` and `shell.policy_sets` in `config.toml`
- `--policies` now activates named policy files; when no names are supplied, the `default` policy is loaded automatically if present
- `zo +init-config` now creates both `~/.config/zo/config.toml` and an example `~/.config/zo/policies/default` policy file
- Legacy `shell.always_on` and `shell.policy_sets` configuration now produces migration errors with guidance to the policy directory
- Session history is trimmed by complete oldest turns when needed to fit the configured serialized-session limit; an oversized current turn is rejected instead of truncated
- Chat retries now preserve a pending turn rather than adding the user message again, and distinguish initial stream failures, partial streams, and tool-call continuation after the per-batch round limit

### Improved
- Safer handling of large inputs with byte-accurate UTF-8 limits and clear source-specific errors
- More reliable retry behavior for failed or incomplete streams and multi-batch tool-call turns
- Unicode-safe debug previews for prompts and STDIN
- Shell policy parsing, validation, matching, approval-rule suggestions, and test coverage
- Shell implementation split into focused parser, policy, executor, and test modules

## [0.5.0]

### Added
- OpenRouter server-side web search support via `--web`
- `web` configuration option to enable server-side web search by default
- Debug output now shows whether web search is enabled

### Changed
- Updated `openrouter-rs` to a newer API with namespaced client calls such as `.models()` and `.chat()`
- `--web` is disallowed together with `--image` to keep image generation and web-enabled chat flows separate

### Improved
- Networking stack updated through the newer `openrouter-rs` release, including removal of the non-Windows vendored `openssl` dependency
- Config initialization now includes the `web` setting and example documentation

## [0.4.0]

### Added
- Shell execution tools with `run_program` and `run_shell_command`
- Configurable shell security policies with `allow`, `ask`, and `deny` actions, including program, argument, command-glob, and command-regex matching
- `--files <read|write>`, `--shell`, and `--policies` CLI flags for independently enabling file and shell tool access
- `--non-interactive` mode to suppress approval prompts and deny approval-required actions automatically
- `--accept-writes` flag to auto-approve file overwrites and edits
- Image generation mode with `--image <FILE>`
- Binary output support via validated image file writing for generated images
- Automatic image modality derivation from model capabilities, with a built-in image-capable fallback model
- Stable `session_id` attached to API requests and reused across a session
- Shell configuration section in config with allowed shells, always-on rules, and named policy sets
- Approval prompt suggestions for durable shell policy rules

### Changed
- Replaced `--tools ro|rw` with separate `--files` and `--shell` controls
- Renamed `--yes` to `--accept-writes`
- Chat/session flow now supports shell runtime integration and non-interactive behavior
- Default text model updated from `openai/gpt-5.3-codex` to `openai/gpt-5.4`
- Default model aliases updated, including newer Gemini and Grok mappings and a built-in image alias
- Model alias configuration now extends built-ins by default, supports overriding built-in aliases, and allows disabling built-in aliases with empty values
- Model resolution refactored into separate text and image paths with more robust built-in fallback behavior
- Config initialization now renders dynamically from model constants and includes richer examples and shell policy documentation
- Confirmation prompts now default to yes (`[Y/n]`) for retries, file overwrites, and edits
- Diff rendering now uses grouped hunks with context headers and preserves missing-newline hints
- Debug output expanded to show file/shell tool access, active shell policies, overwrite approval mode, and interactive confirmation state
- System prompt generation now includes shell-tool instructions when shell access is enabled

### Improved
- More stable fuzzy model matching with deterministic ordering and clearer precedence between custom, built-in, and added aliases
- Better handling of hidden paths and workspace enforcement for shell cwd and binary output paths
- Safer non-interactive behavior for initial requests, chat retries, file overwrites, and binary writes
- Clearer config semantics and docs for built-in model aliases and shell execution
- Image generation flow reuse through extracted `run_image_mode`

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
