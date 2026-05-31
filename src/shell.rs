use anyhow::{Context, Result, bail};
use crossterm::ExecutableCommand;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use glob::Pattern;
use openrouter_rs::types::typed_tool::TypedTool;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::{
    InlineColors, ShellArgMatcher, ShellConfig, ShellPolicyAction, ShellPolicyEntry, ShellPolicySet,
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 24_000;

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct RunProgramParams {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_output: Option<usize>,
}

impl TypedTool for RunProgramParams {
    fn name() -> &'static str {
        "run_program"
    }

    fn description() -> &'static str {
        "Execute one program with explicit arguments. Prefer this over run_shell_command unless you need shell syntax like pipelines."
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(deny_unknown_fields)]
pub struct RunShellCommandParams {
    pub command: String,
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub timeout_ms: Option<u64>,
    pub max_output: Option<usize>,
}

impl TypedTool for RunShellCommandParams {
    fn name() -> &'static str {
        "run_shell_command"
    }

    fn description() -> &'static str {
        "Execute a full shell command line with an allowlisted shell. Use this only when you truly need shell syntax like pipelines."
    }
}

#[derive(Debug, Clone)]
pub struct ShellRuntime {
    default_action: ShellPolicyAction,
    allowed_shells: Vec<String>,
    non_interactive: bool,
    show_verbose_approval_details: bool,
    always_on: Vec<ShellPolicyEntry>,
    selected_sets: Vec<ShellPolicySet>,
}

#[derive(Debug, Clone)]
struct NormalizedSegment {
    program: String,
    args: Vec<String>,
}

#[derive(Debug, Clone)]
enum ExecutionRequest {
    Program { program: String, args: Vec<String> },
    Shell { shell_path: String, command: String },
}

#[derive(Debug, Clone)]
struct NormalizedRequest {
    execution: ExecutionRequest,
    normalized_command: String,
    cwd_path: PathBuf,
    cwd_display: String,
    timeout_ms: u64,
    max_output: usize,
    segments: Vec<NormalizedSegment>,
    shell_path: Option<String>,
    resolved_programs: Vec<String>,
    requires_approval: bool,
    gate_reasons: Vec<String>,
    rejection: Option<Rejection>,
    fingerprint: String,
}

#[derive(Debug, Clone)]
struct Rejection {
    kind: String,
    reason: String,
}

#[derive(Debug, Clone)]
struct PolicyDecision {
    action: ShellPolicyAction,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovalPrompt {
    command: String,
    metadata_line: String,
    detail_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DurableRuleSuggestion {
    heading: &'static str,
    rule: String,
}

#[derive(Debug, Clone, Default)]
struct SegmentPolicyState {
    action: Option<ShellPolicyAction>,
    summary: Option<String>,
    locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Pipe,
    AndIf,
    OrIf,
    Semicolon,
    RedirectIn,
    RedirectOut,
    RedirectAppend,
}

impl Operator {
    fn text(self) -> &'static str {
        match self {
            Self::Pipe => "|",
            Self::AndIf => "&&",
            Self::OrIf => "||",
            Self::Semicolon => ";",
            Self::RedirectIn => "<",
            Self::RedirectOut => ">",
            Self::RedirectAppend => ">>",
        }
    }
}

#[derive(Debug, Clone)]
struct Token {
    value: String,
    has_unquoted_glob: bool,
    has_variable_expansion: bool,
    has_parentheses: bool,
}

#[derive(Debug, Clone)]
enum LexItem {
    Word(Token),
    Operator(Operator),
}

#[derive(Debug, Clone)]
struct ParsedSegment {
    program: String,
    args: Vec<String>,
    env_assignments: Vec<String>,
}

#[derive(Debug, Clone)]
struct ParsedShellCommand {
    normalized_command: String,
    segments: Vec<ParsedSegment>,
    gate_reasons: Vec<String>,
    rejection: Option<Rejection>,
}

#[derive(Debug, Serialize)]
struct ShellToolResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    normalized_command: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    timed_out: bool,
    output_truncated: bool,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u128>,
}

#[derive(Debug)]
struct ExecutionOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    output_truncated: bool,
    stdout: String,
    stderr: String,
    duration_ms: u128,
}

impl ShellRuntime {
    pub fn new(
        config: &ShellConfig,
        active_policy_sets: &[String],
        non_interactive: bool,
        show_verbose_approval_details: bool,
    ) -> Result<Self> {
        let mut seen = HashSet::new();
        let mut selected_sets = Vec::new();

        for set_name in active_policy_sets {
            let normalized = set_name.to_ascii_lowercase();
            if !seen.insert(normalized.clone()) {
                bail!("--policies contains duplicate set name '{}'", set_name);
            }

            let Some(policy_set) = config
                .policy_sets
                .iter()
                .find(|set| set.name.eq_ignore_ascii_case(set_name))
            else {
                bail!("Unknown shell policy set '{}'", set_name);
            };

            selected_sets.push(policy_set.clone());
        }

        Ok(Self {
            default_action: config.default_action,
            allowed_shells: config.allowed_shells.clone(),
            non_interactive,
            show_verbose_approval_details,
            always_on: config.always_on.clone(),
            selected_sets,
        })
    }

    pub async fn execute_program(
        &self,
        params: RunProgramParams,
        allow_hidden: bool,
        inline_colors: &InlineColors,
        retry_guard: &mut HashSet<String>,
    ) -> Result<String> {
        let normalized = self.normalize_program_request(params, allow_hidden)?;
        self.execute_normalized_request(normalized, inline_colors, retry_guard)
            .await
    }

    pub async fn execute_shell_command(
        &self,
        params: RunShellCommandParams,
        allow_hidden: bool,
        inline_colors: &InlineColors,
        retry_guard: &mut HashSet<String>,
    ) -> Result<String> {
        let normalized = self.normalize_shell_request(params, allow_hidden)?;
        self.execute_normalized_request(normalized, inline_colors, retry_guard)
            .await
    }

    async fn execute_normalized_request(
        &self,
        request: NormalizedRequest,
        inline_colors: &InlineColors,
        retry_guard: &mut HashSet<String>,
    ) -> Result<String> {
        if let Some(rejection) = &request.rejection {
            retry_guard.insert(request.fingerprint.clone());
            return serialize_response(&ShellToolResponse {
                ok: false,
                kind: Some(rejection.kind.clone()),
                reason: Some(rejection.reason.clone()),
                normalized_command: request.normalized_command,
                cwd: request.cwd_display,
                shell: request.shell_path,
                exit_code: None,
                timed_out: false,
                output_truncated: false,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: None,
            });
        }

        if retry_guard.contains(&request.fingerprint) {
            return serialize_response(&ShellToolResponse {
                ok: false,
                kind: Some("retry_guard".to_string()),
                reason: Some(
                    "The same command was already denied or rejected in this session.".to_string(),
                ),
                normalized_command: request.normalized_command,
                cwd: request.cwd_display,
                shell: request.shell_path,
                exit_code: None,
                timed_out: false,
                output_truncated: false,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: None,
            });
        }

        let policy = self.evaluate_policy(&request);
        let action = if request.requires_approval && policy.action == ShellPolicyAction::Allow {
            ShellPolicyAction::Ask
        } else {
            policy.action
        };

        if action == ShellPolicyAction::Deny {
            retry_guard.insert(request.fingerprint.clone());
            return serialize_response(&ShellToolResponse {
                ok: false,
                kind: Some("policy_denied".to_string()),
                reason: Some(policy.reason),
                normalized_command: request.normalized_command,
                cwd: request.cwd_display,
                shell: request.shell_path,
                exit_code: None,
                timed_out: false,
                output_truncated: false,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: None,
            });
        }

        if action == ShellPolicyAction::Ask && self.non_interactive {
            retry_guard.insert(request.fingerprint.clone());
            return serialize_response(&ShellToolResponse {
                ok: false,
                kind: Some("approval_denied".to_string()),
                reason: Some(format!(
                    "{} Approval was denied because --non-interactive is enabled.",
                    policy.reason
                )),
                normalized_command: request.normalized_command,
                cwd: request.cwd_display,
                shell: request.shell_path,
                exit_code: None,
                timed_out: false,
                output_truncated: false,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: None,
            });
        }

        if action == ShellPolicyAction::Ask
            && !self.prompt_for_approval(&request, &policy.reason, inline_colors)?
        {
            retry_guard.insert(request.fingerprint.clone());
            return serialize_response(&ShellToolResponse {
                ok: false,
                kind: Some("user_denied".to_string()),
                reason: Some("The user denied the command.".to_string()),
                normalized_command: request.normalized_command,
                cwd: request.cwd_display,
                shell: request.shell_path,
                exit_code: None,
                timed_out: false,
                output_truncated: false,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: None,
            });
        }

        let outcome = execute_request(&request).await?;
        serialize_response(&ShellToolResponse {
            ok: true,
            kind: None,
            reason: None,
            normalized_command: request.normalized_command,
            cwd: request.cwd_display,
            shell: request.shell_path,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
            output_truncated: outcome.output_truncated,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            duration_ms: Some(outcome.duration_ms),
        })
    }

    fn normalize_program_request(
        &self,
        params: RunProgramParams,
        allow_hidden: bool,
    ) -> Result<NormalizedRequest> {
        if params.program.trim().is_empty() {
            bail!("run_program.program must not be empty");
        }

        let (cwd_path, cwd_display, cwd_gate_reason) =
            normalize_cwd(params.cwd.as_deref(), allow_hidden)?;
        let timeout_ms = clamp_timeout(params.timeout_ms);
        let max_output = clamp_max_output(params.max_output);
        let resolved_program = resolve_executable(&params.program, &cwd_path);

        if let Some(shell_path) =
            self.detect_shell_wrapper(&params.program, &params.args, resolved_program.as_deref())
        {
            let shell_command = params
                .args
                .get(1)
                .cloned()
                .context("Shell wrapper requires a command string argument")?;
            let parsed = match parse_shell_command(&shell_command) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(rejected_request(
                        "unsupported_shell_syntax",
                        err.to_string(),
                        &shell_command,
                        cwd_path,
                        cwd_display,
                        timeout_ms,
                        max_output,
                        Some(shell_path),
                    ));
                }
            };
            return Ok(self.finish_shell_request(
                ExecutionRequest::Program {
                    program: params.program,
                    args: params.args,
                },
                shell_path,
                parsed,
                cwd_path,
                cwd_display,
                timeout_ms,
                max_output,
                allow_hidden,
                cwd_gate_reason,
            ));
        }

        let mut gate_reasons = Vec::new();
        if let Some(reason) = cwd_gate_reason {
            gate_reasons.push(reason);
        }

        let normalized_program = normalize_token_for_display(
            &params.program,
            &cwd_path,
            allow_hidden,
            &mut gate_reasons,
        );
        let normalized_args = params
            .args
            .iter()
            .map(|arg| normalize_token_for_display(arg, &cwd_path, allow_hidden, &mut gate_reasons))
            .collect::<Vec<_>>();
        let normalized_command = join_command_words(
            std::iter::once(normalized_program.clone()).chain(normalized_args.iter().cloned()),
        );

        let segment = NormalizedSegment {
            program: basename_for_policy(&normalized_program),
            args: normalized_args,
        };

        let fingerprint = build_fingerprint(&normalized_command, &cwd_display, None);

        Ok(NormalizedRequest {
            execution: ExecutionRequest::Program {
                program: params.program,
                args: params.args,
            },
            normalized_command,
            cwd_path,
            cwd_display,
            timeout_ms,
            max_output,
            segments: vec![segment],
            shell_path: None,
            resolved_programs: resolved_program.into_iter().collect(),
            requires_approval: !gate_reasons.is_empty(),
            gate_reasons,
            rejection: None,
            fingerprint,
        })
    }

    fn normalize_shell_request(
        &self,
        params: RunShellCommandParams,
        allow_hidden: bool,
    ) -> Result<NormalizedRequest> {
        if params.command.trim().is_empty() {
            bail!("run_shell_command.command must not be empty");
        }

        let (cwd_path, cwd_display, cwd_gate_reason) =
            normalize_cwd(params.cwd.as_deref(), allow_hidden)?;
        let timeout_ms = clamp_timeout(params.timeout_ms);
        let max_output = clamp_max_output(params.max_output);
        let shell_path = match params.shell {
            Some(shell) => {
                if !self.allowed_shells.iter().any(|allowed| allowed == &shell) {
                    return Ok(rejected_request(
                        "shell_not_allowed",
                        format!(
                            "Requested shell '{}' is not in the configured allowlist.",
                            shell
                        ),
                        &params.command,
                        cwd_path,
                        cwd_display,
                        timeout_ms,
                        max_output,
                        Some(shell),
                    ));
                }
                shell
            }
            None => self
                .allowed_shells
                .first()
                .cloned()
                .context("No shells configured in shell.allowed_shells")?,
        };
        let parsed = match parse_shell_command(&params.command) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Ok(rejected_request(
                    "unsupported_shell_syntax",
                    err.to_string(),
                    &params.command,
                    cwd_path,
                    cwd_display,
                    timeout_ms,
                    max_output,
                    Some(shell_path.clone()),
                ));
            }
        };

        Ok(self.finish_shell_request(
            ExecutionRequest::Shell {
                shell_path: shell_path.clone(),
                command: params.command,
            },
            shell_path,
            parsed,
            cwd_path,
            cwd_display,
            timeout_ms,
            max_output,
            allow_hidden,
            cwd_gate_reason,
        ))
    }

    fn finish_shell_request(
        &self,
        execution: ExecutionRequest,
        shell_path: String,
        parsed: ParsedShellCommand,
        cwd_path: PathBuf,
        cwd_display: String,
        timeout_ms: u64,
        max_output: usize,
        allow_hidden: bool,
        cwd_gate_reason: Option<String>,
    ) -> NormalizedRequest {
        let mut gate_reasons = parsed.gate_reasons;
        if let Some(reason) = cwd_gate_reason {
            gate_reasons.push(reason);
        }

        let mut segments = Vec::new();
        let mut resolved_programs = Vec::new();

        for parsed_segment in &parsed.segments {
            let normalized_program = normalize_token_for_display(
                &parsed_segment.program,
                &cwd_path,
                allow_hidden,
                &mut gate_reasons,
            );
            let normalized_args = parsed_segment
                .args
                .iter()
                .map(|arg| {
                    normalize_token_for_display(arg, &cwd_path, allow_hidden, &mut gate_reasons)
                })
                .collect::<Vec<_>>();
            let resolved = resolve_executable(&parsed_segment.program, &cwd_path);
            if let Some(path) = &resolved {
                resolved_programs.push(path.clone());
            }

            for assignment in &parsed_segment.env_assignments {
                gate_reasons.push(format!(
                    "Uses inline environment assignment '{}', which always requires approval.",
                    assignment
                ));
            }

            segments.push(NormalizedSegment {
                program: basename_for_policy(&normalized_program),
                args: normalized_args,
            });
        }

        dedupe_preserve_order(&mut gate_reasons);
        let fingerprint =
            build_fingerprint(&parsed.normalized_command, &cwd_display, Some(&shell_path));

        NormalizedRequest {
            execution,
            normalized_command: parsed.normalized_command,
            cwd_path,
            cwd_display,
            timeout_ms,
            max_output,
            segments,
            shell_path: Some(shell_path),
            resolved_programs,
            requires_approval: !gate_reasons.is_empty(),
            gate_reasons,
            rejection: parsed.rejection,
            fingerprint,
        }
    }

    fn detect_shell_wrapper(
        &self,
        program: &str,
        args: &[String],
        resolved_program: Option<&str>,
    ) -> Option<String> {
        if args.len() != 2 {
            return None;
        }

        if args[0] != "-c" && args[0] != "-lc" {
            return None;
        }

        let candidate = resolved_program.unwrap_or(program);
        if self.allowed_shells.iter().any(|shell| shell == candidate) {
            Some(candidate.to_string())
        } else {
            None
        }
    }

    fn evaluate_policy(&self, request: &NormalizedRequest) -> PolicyDecision {
        let mut segment_states = vec![SegmentPolicyState::default(); request.segments.len()];

        for entry in self
            .always_on
            .iter()
            .chain(self.selected_sets.iter().flat_map(|set| set.entries.iter()))
        {
            if let Some(action) = match_command_entry(entry, request) {
                let summary = entry_summary(entry);
                for state in &mut segment_states {
                    if state.locked {
                        continue;
                    }
                    state.action = Some(action);
                    state.summary = Some(summary.clone());
                    if entry.terminal {
                        state.locked = true;
                    }
                }
                continue;
            }

            let summary = entry_summary(entry);
            for (index, segment) in request.segments.iter().enumerate() {
                let state = &mut segment_states[index];
                if state.locked || !match_program_entry(entry, segment) {
                    continue;
                }

                state.action = Some(entry.action);
                state.summary = Some(summary.clone());
                if entry.terminal {
                    state.locked = true;
                }
            }
        }

        if request.segments.len() == 1 {
            if let Some(action) = segment_states[0].action {
                return PolicyDecision {
                    action,
                    reason: format!(
                        "Matched policy entry: {}",
                        segment_states[0].summary.as_deref().unwrap_or_default()
                    ),
                };
            }

            return PolicyDecision {
                action: self.default_action,
                reason: format!(
                    "No policy entry matched. The default shell action is '{}'.",
                    action_name(self.default_action)
                ),
            };
        }

        let actions = segment_states
            .into_iter()
            .map(|state| (state.action.unwrap_or(self.default_action), state.summary))
            .collect::<Vec<_>>();

        let action = if actions
            .iter()
            .any(|(action, _)| *action == ShellPolicyAction::Deny)
        {
            ShellPolicyAction::Deny
        } else if actions
            .iter()
            .all(|(action, _)| *action == ShellPolicyAction::Allow)
        {
            ShellPolicyAction::Allow
        } else {
            ShellPolicyAction::Ask
        };

        let all_default = actions.iter().all(|(_, summary)| summary.is_none());
        let details = actions
            .iter()
            .enumerate()
            .map(|(index, (segment_action, summary))| {
                if let Some(summary) = summary {
                    format!(
                        "segment {} matched {} -> '{}'",
                        index + 1,
                        summary,
                        action_name(*segment_action)
                    )
                } else {
                    format!(
                        "segment {} used default '{}'",
                        index + 1,
                        action_name(self.default_action)
                    )
                }
            })
            .collect::<Vec<_>>();

        PolicyDecision {
            action,
            reason: if all_default && action == self.default_action {
                format!(
                    "Pipeline commands fell back to the default shell action '{}'.",
                    action_name(self.default_action)
                )
            } else {
                format!(
                    "Pipeline commands resolved to shell action '{}': {}.",
                    action_name(action),
                    details.join("; ")
                )
            },
        }
    }

    fn prompt_for_approval(
        &self,
        request: &NormalizedRequest,
        policy_reason: &str,
        inline_colors: &InlineColors,
    ) -> Result<bool> {
        let prompt =
            build_approval_prompt(request, policy_reason, self.show_verbose_approval_details);

        let mut stdout = io::stdout();
        stdout.execute(Print("\nApprove shell command?\n"))?;

        for line in prompt.command.lines() {
            stdout
                .execute(Print("    "))?
                .execute(SetForegroundColor(inline_colors.get_inline_code_color()))?
                .execute(SetAttribute(Attribute::Bold))?
                .execute(Print(line))?
                .execute(ResetColor)?
                .execute(SetAttribute(Attribute::Reset))?
                .execute(Print("\n"))?;
        }

        stdout
            .execute(Print("    "))?
            .execute(Print(prompt.metadata_line))?
            .execute(Print("\n"))?;

        for line in &prompt.detail_lines {
            stdout
                .execute(Print("    "))?
                .execute(Print(line))?
                .execute(Print("\n"))?;
        }

        stdout.execute(Print("Allow command? [Y/n]: "))?;
        stdout.flush().context("Failed to flush stdout")?;
        Ok(is_confirmation_approved(&read_confirmation_response()?))
    }
}

fn rejected_request(
    kind: impl Into<String>,
    reason: impl Into<String>,
    normalized_command: &str,
    cwd_path: PathBuf,
    cwd_display: String,
    timeout_ms: u64,
    max_output: usize,
    shell_path: Option<String>,
) -> NormalizedRequest {
    let fingerprint = build_fingerprint(normalized_command, &cwd_display, shell_path.as_deref());

    NormalizedRequest {
        execution: ExecutionRequest::Program {
            program: String::new(),
            args: Vec::new(),
        },
        normalized_command: normalized_command.trim().to_string(),
        cwd_path,
        cwd_display,
        timeout_ms,
        max_output,
        segments: Vec::new(),
        shell_path,
        resolved_programs: Vec::new(),
        requires_approval: false,
        gate_reasons: Vec::new(),
        rejection: Some(Rejection {
            kind: kind.into(),
            reason: reason.into(),
        }),
        fingerprint,
    }
}

fn serialize_response(response: &ShellToolResponse) -> Result<String> {
    serde_json::to_string_pretty(response).context("Failed to serialize shell tool response")
}

fn action_name(action: ShellPolicyAction) -> &'static str {
    match action {
        ShellPolicyAction::Allow => "allow",
        ShellPolicyAction::Ask => "ask",
        ShellPolicyAction::Deny => "deny",
    }
}

fn match_command_entry(
    entry: &ShellPolicyEntry,
    request: &NormalizedRequest,
) -> Option<ShellPolicyAction> {
    if let Some(pattern) = &entry.command_glob {
        return Pattern::new(pattern)
            .ok()
            .filter(|glob| glob.matches(&request.normalized_command))
            .map(|_| entry.action);
    }

    if let Some(pattern) = &entry.command_regex {
        return compile_full_match_regex(pattern)
            .ok()
            .filter(|regex| regex.is_match(&request.normalized_command))
            .map(|_| entry.action);
    }

    None
}

fn match_program_entry(entry: &ShellPolicyEntry, segment: &NormalizedSegment) -> bool {
    let Some(program) = &entry.program else {
        return false;
    };

    if basename_for_policy(program) != segment.program {
        return false;
    }

    if !entry.args.is_empty() {
        if entry.args.len() != segment.args.len() {
            return false;
        }

        return entry
            .args
            .iter()
            .zip(segment.args.iter())
            .all(|(matcher, arg)| arg_matches(matcher, arg));
    }

    if !entry.args_prefix.is_empty() {
        if segment.args.len() < entry.args_prefix.len() {
            return false;
        }

        return entry
            .args_prefix
            .iter()
            .zip(segment.args.iter())
            .all(|(matcher, arg)| arg_matches(matcher, arg));
    }

    segment.args.is_empty()
}

fn arg_matches(matcher: &ShellArgMatcher, arg: &str) -> bool {
    if let Some(expected) = &matcher.exact {
        return expected == arg;
    }
    if let Some(pattern) = &matcher.glob {
        return Pattern::new(pattern)
            .map(|glob| glob.matches(arg))
            .unwrap_or(false);
    }
    if let Some(pattern) = &matcher.regex {
        return compile_full_match_regex(pattern)
            .map(|regex| regex.is_match(arg))
            .unwrap_or(false);
    }
    false
}

fn compile_full_match_regex(pattern: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!(r"\A(?:{})\z", pattern))
}

fn entry_summary(entry: &ShellPolicyEntry) -> String {
    let summary = if let Some(program) = &entry.program {
        if entry.args.is_empty() && entry.args_prefix.is_empty() {
            format!("program='{}'", basename_for_policy(program))
        } else {
            let args = if !entry.args.is_empty() {
                format!("args=[{}]", format_arg_matchers(&entry.args))
            } else {
                format!("args_prefix=[{}]", format_arg_matchers(&entry.args_prefix))
            };
            format!("program='{}' {}", basename_for_policy(program), args)
        }
    } else if let Some(pattern) = &entry.command_glob {
        format!("command_glob={}", quote_token(pattern))
    } else {
        format!(
            "command_regex={}",
            quote_token(entry.command_regex.as_deref().unwrap_or_default())
        )
    };

    if entry.terminal {
        format!("{} terminal=true", summary)
    } else {
        summary
    }
}

fn build_approval_prompt(
    request: &NormalizedRequest,
    policy_reason: &str,
    show_verbose_approval_details: bool,
) -> ApprovalPrompt {
    let mut metadata = Vec::new();
    if let Some(shell) = &request.shell_path {
        metadata.push(format!("shell: {}", shell));
    }
    if request.cwd_display != "." {
        metadata.push(format!("cwd: {}", request.cwd_display));
    }
    metadata.push(format!("timeout: {} ms", request.timeout_ms));
    metadata.push(format!("output: {} chars", request.max_output));

    let mut detail_lines = Vec::new();
    if show_verbose_approval_details {
        detail_lines.push(format!("Reason: {}", policy_reason));

        if !request.gate_reasons.is_empty() {
            detail_lines.push(format!("Gates: {}", request.gate_reasons.join("; ")));
        }

        if !request.resolved_programs.is_empty() {
            detail_lines.push(format!(
                "Executables: {}",
                request.resolved_programs.join(", ")
            ));
        }

        for suggestion in suggested_rules(request) {
            detail_lines.push(suggestion.heading.to_string());
            detail_lines.extend(suggestion.rule.lines().map(|line| format!("  {}", line)));
        }
    }

    ApprovalPrompt {
        command: request.normalized_command.clone(),
        metadata_line: metadata.join(" · "),
        detail_lines,
    }
}

fn suggested_rules(request: &NormalizedRequest) -> Vec<DurableRuleSuggestion> {
    let mut suggestions = Vec::new();
    if let Some(rule) = suggested_rule(request) {
        suggestions.push(DurableRuleSuggestion {
            heading: "Exact durable rule:",
            rule,
        });
    }
    if let Some(rule) = suggested_family_rule(request) {
        suggestions.push(DurableRuleSuggestion {
            heading: "Family durable rule (broader):",
            rule,
        });
    }
    suggestions
}

fn suggested_rule(request: &NormalizedRequest) -> Option<String> {
    if request.segments.len() != 1 {
        return None;
    }

    let segment = request.segments.first()?;
    let args = segment
        .args
        .iter()
        .map(|arg| format!("{{ exact = {} }}", quote_toml_string(arg)))
        .collect::<Vec<_>>();

    Some(exact_rule_for_segment(&segment.program, &args))
}

fn suggested_family_rule(request: &NormalizedRequest) -> Option<String> {
    if request.segments.len() != 1 {
        return None;
    }

    let segment = request.segments.first()?;
    let first_arg = segment.args.first()?;
    if first_arg.starts_with('-') {
        return None;
    }

    let prefix = format!("{{ exact = {} }}", quote_toml_string(first_arg));
    Some(format!(
        "program = {}\nargs_prefix = [{}]",
        quote_toml_string(&segment.program),
        prefix
    ))
}

fn format_arg_matchers(matchers: &[ShellArgMatcher]) -> String {
    matchers
        .iter()
        .map(|matcher| {
            if let Some(value) = &matcher.exact {
                format!("exact({})", quote_token(value))
            } else if let Some(value) = &matcher.glob {
                format!("glob({})", quote_token(value))
            } else {
                format!(
                    "regex({})",
                    quote_token(matcher.regex.as_deref().unwrap_or_default())
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn exact_rule_for_segment(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        format!("program = {}", quote_toml_string(program))
    } else {
        format!(
            "program = {}\nargs = [{}]",
            quote_toml_string(program),
            args.join(", ")
        )
    }
}

fn clamp_timeout(value: Option<u64>) -> u64 {
    value.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS)
}

fn clamp_max_output(value: Option<usize>) -> usize {
    value
        .unwrap_or(DEFAULT_MAX_OUTPUT_CHARS)
        .clamp(1, DEFAULT_MAX_OUTPUT_CHARS)
}

fn build_fingerprint(
    normalized_command: &str,
    cwd_display: &str,
    shell_path: Option<&str>,
) -> String {
    format!(
        "{}|{}|{}",
        normalized_command,
        cwd_display,
        shell_path.unwrap_or_default(),
    )
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn basename_for_policy(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string())
}

fn workspace_root() -> Result<PathBuf> {
    env::current_dir()
        .context("Failed to resolve current working directory")?
        .canonicalize()
        .context("Failed to canonicalize current working directory")
}

fn normalize_cwd(
    cwd: Option<&str>,
    allow_hidden: bool,
) -> Result<(PathBuf, String, Option<String>)> {
    let root = workspace_root()?;
    let requested = cwd.unwrap_or(".");
    let requested_path = PathBuf::from(requested);

    if requested_path.is_absolute() {
        bail!(
            "Shell tool cwd must be relative to the current workspace: {}",
            requested
        );
    }

    let joined = root.join(&requested_path);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("Shell tool cwd does not exist: {}", requested))?;

    if !canonical.starts_with(&root) {
        bail!(
            "Shell tool cwd resolves outside the current workspace: {}",
            requested
        );
    }
    if !canonical.is_dir() {
        bail!("Shell tool cwd is not a directory: {}", requested);
    }

    let relative = relative_display_path(&canonical, &root)?;
    let gate_reason = if !allow_hidden
        && has_hidden_component(
            canonical
                .strip_prefix(&root)
                .unwrap_or_else(|_| Path::new(".")),
        ) {
        Some(format!(
            "Targets hidden working directory '{}'; hidden paths require approval without --hidden.",
            relative
        ))
    } else {
        None
    };

    Ok((canonical, relative, gate_reason))
}

fn normalize_token_for_display(
    token: &str,
    cwd_path: &Path,
    allow_hidden: bool,
    gate_reasons: &mut Vec<String>,
) -> String {
    let Some(path_like) = normalize_explicit_path(token, cwd_path) else {
        return token.to_string();
    };

    match path_like {
        ExplicitPath::InWorkspace {
            display,
            touches_hidden,
        } => {
            if touches_hidden && !allow_hidden {
                gate_reasons.push(format!(
                    "Touches hidden path '{}'; hidden paths require approval without --hidden.",
                    display
                ));
            }
            display
        }
        ExplicitPath::External(display) => {
            gate_reasons.push(format!(
                "Touches explicit path outside the workspace: {}",
                display
            ));
            display
        }
    }
}

enum ExplicitPath {
    InWorkspace {
        display: String,
        touches_hidden: bool,
    },
    External(String),
}

fn normalize_explicit_path(token: &str, cwd_path: &Path) -> Option<ExplicitPath> {
    if !looks_like_explicit_path(token) {
        return None;
    }

    if token.starts_with('~') {
        return Some(ExplicitPath::External(token.to_string()));
    }

    let root = workspace_root().ok()?;
    let candidate = if Path::new(token).is_absolute() {
        PathBuf::from(token)
    } else {
        cwd_path.join(token)
    };

    let normalized = if candidate.exists() {
        candidate.canonicalize().ok()
    } else {
        let parent = candidate.parent()?;
        let canonical_parent = parent.canonicalize().ok()?;
        candidate
            .file_name()
            .map(|file_name| canonical_parent.join(file_name))
    };

    let Some(path) = normalized else {
        return Some(ExplicitPath::External(
            candidate.to_string_lossy().to_string(),
        ));
    };

    if !path.starts_with(&root) {
        return Some(ExplicitPath::External(path.to_string_lossy().to_string()));
    }

    let relative = path.strip_prefix(&root).ok()?;
    Some(ExplicitPath::InWorkspace {
        display: if relative.as_os_str().is_empty() {
            ".".to_string()
        } else {
            relative.to_string_lossy().to_string()
        },
        touches_hidden: has_hidden_component(relative),
    })
}

fn looks_like_explicit_path(token: &str) -> bool {
    token == "."
        || token == ".."
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
        || token.contains('/')
}

fn relative_display_path(path: &Path, root: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("Path not under workspace root: {}", path.display()))?;
    if relative.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(relative.to_string_lossy().to_string())
    }
}

fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn resolve_executable(program: &str, cwd_path: &Path) -> Option<String> {
    if program.contains('/') {
        let candidate = if Path::new(program).is_absolute() {
            PathBuf::from(program)
        } else {
            cwd_path.join(program)
        };
        return candidate
            .canonicalize()
            .ok()
            .map(|path| path.to_string_lossy().to_string());
    }

    let path_value = env::var("PATH").ok()?;

    env::split_paths(&path_value)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().to_string())
}

fn join_command_words<I>(words: I) -> String
where
    I: IntoIterator<Item = String>,
{
    words
        .into_iter()
        .map(|word| quote_token(&word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_token(token: &str) -> String {
    if token.is_empty() {
        return "''".to_string();
    }

    if token
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':'))
    {
        return token.to_string();
    }

    format!("'{}'", token.replace('\'', r#"'\"'\"'"#))
}

fn quote_toml_string(token: &str) -> String {
    let mut quoted = String::with_capacity(token.len() + 2);
    quoted.push('"');

    for ch in token.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\u{08}' => quoted.push_str("\\b"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\u{0C}' => quoted.push_str("\\f"),
            '\r' => quoted.push_str("\\r"),
            _ if ch.is_control() => {
                write!(&mut quoted, "\\u{:04X}", ch as u32)
                    .expect("writing to a String should never fail");
            }
            _ => quoted.push(ch),
        }
    }

    quoted.push('"');
    quoted
}

fn dedupe_preserve_order(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn parse_shell_command(command: &str) -> Result<ParsedShellCommand> {
    if command.contains('\n') {
        return Ok(ParsedShellCommand {
            normalized_command: command.trim().to_string(),
            segments: Vec::new(),
            gate_reasons: Vec::new(),
            rejection: Some(Rejection {
                kind: "unsupported_shell_syntax".to_string(),
                reason: "Multiline shell input is not supported.".to_string(),
            }),
        });
    }

    let items = lex_shell(command)?;
    let mut normalized_parts = Vec::new();
    let mut segments = Vec::new();
    let mut current_tokens = Vec::new();
    let mut current_has_redirection = false;
    let mut gate_reasons = Vec::new();
    for item in items {
        match item {
            LexItem::Word(token) => {
                if token.has_unquoted_glob {
                    gate_reasons.push(format!(
                        "Uses wildcard expansion in '{}', which always requires approval.",
                        token.value
                    ));
                }
                if token.has_variable_expansion {
                    gate_reasons.push(format!(
                        "Uses shell variable expansion in '{}', which requires approval.",
                        token.value
                    ));
                }
                if token.has_parentheses {
                    gate_reasons.push(format!(
                        "Uses shell grouping syntax in '{}', which requires approval.",
                        token.value
                    ));
                }
                normalized_parts.push(quote_token(&token.value));
                current_tokens.push(token);
            }
            LexItem::Operator(operator) => {
                match operator {
                    Operator::Pipe => {
                        push_segment(&mut segments, &mut current_tokens, current_has_redirection)?;
                        current_has_redirection = false;
                    }
                    Operator::AndIf => {
                        gate_reasons.push(
                            "Uses '&&' command chaining, which always requires approval."
                                .to_string(),
                        );
                        push_segment(&mut segments, &mut current_tokens, current_has_redirection)?;
                        current_has_redirection = false;
                    }
                    Operator::OrIf => {
                        gate_reasons.push(
                            "Uses '||' command chaining, which always requires approval."
                                .to_string(),
                        );
                        push_segment(&mut segments, &mut current_tokens, current_has_redirection)?;
                        current_has_redirection = false;
                    }
                    Operator::Semicolon => {
                        gate_reasons.push(
                            "Uses ';' command sequencing, which always requires approval."
                                .to_string(),
                        );
                        push_segment(&mut segments, &mut current_tokens, current_has_redirection)?;
                        current_has_redirection = false;
                    }
                    Operator::RedirectIn | Operator::RedirectOut | Operator::RedirectAppend => {
                        current_has_redirection = true;
                        gate_reasons.push(format!(
                            "Uses '{}' redirection, which always requires approval.",
                            operator.text()
                        ));
                    }
                }
                normalized_parts.push(operator.text().to_string());
            }
        }
    }

    push_segment(&mut segments, &mut current_tokens, current_has_redirection)?;
    dedupe_preserve_order(&mut gate_reasons);

    Ok(ParsedShellCommand {
        normalized_command: normalized_parts.join(" "),
        segments,
        gate_reasons,
        rejection: None,
    })
}

fn push_segment(
    segments: &mut Vec<ParsedSegment>,
    current_tokens: &mut Vec<Token>,
    has_redirection: bool,
) -> Result<()> {
    if current_tokens.is_empty() {
        bail!("Shell command contains an empty command segment");
    }

    let mut iter = current_tokens.drain(..).peekable();
    let mut env_assignments = Vec::new();

    while let Some(token) = iter.peek() {
        if is_env_assignment(&token.value) {
            env_assignments.push(token.value.clone());
            iter.next();
            continue;
        }
        break;
    }

    let Some(program_token) = iter.next() else {
        bail!("Shell command segment is missing a program");
    };

    let args = iter.map(|token| token.value).collect::<Vec<_>>();
    let _ = has_redirection;
    segments.push(ParsedSegment {
        program: program_token.value,
        args,
        env_assignments,
    });

    Ok(())
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    is_valid_env_name(name)
}

fn lex_shell(command: &str) -> Result<Vec<LexItem>> {
    let mut chars = command.chars().peekable();
    let mut items = Vec::new();
    let mut current = String::new();
    let mut token_started = false;
    let mut has_unquoted_glob = false;
    let mut has_variable_expansion = false;
    let mut has_parentheses = false;
    enum State {
        Normal,
        SingleQuoted,
        DoubleQuoted,
    }
    let mut state = State::Normal;

    let flush = |items: &mut Vec<LexItem>,
                 current: &mut String,
                 token_started: &mut bool,
                 has_unquoted_glob: &mut bool,
                 has_variable_expansion: &mut bool,
                 has_parentheses: &mut bool| {
        if !*token_started {
            return;
        }
        items.push(LexItem::Word(Token {
            value: current.clone(),
            has_unquoted_glob: *has_unquoted_glob,
            has_variable_expansion: *has_variable_expansion,
            has_parentheses: *has_parentheses,
        }));
        current.clear();
        *token_started = false;
        *has_unquoted_glob = false;
        *has_variable_expansion = false;
        *has_parentheses = false;
    };

    while let Some(ch) = chars.next() {
        match state {
            State::Normal => match ch {
                ' ' | '\t' | '\r' => {
                    flush(
                        &mut items,
                        &mut current,
                        &mut token_started,
                        &mut has_unquoted_glob,
                        &mut has_variable_expansion,
                        &mut has_parentheses,
                    );
                }
                '\'' => {
                    token_started = true;
                    state = State::SingleQuoted;
                }
                '"' => {
                    token_started = true;
                    state = State::DoubleQuoted;
                }
                '\\' => {
                    token_started = true;
                    let Some(next) = chars.next() else {
                        bail!("Shell command ends with a dangling escape");
                    };
                    current.push(next);
                }
                '`' => {
                    bail!("Command substitution with backticks is not supported");
                }
                '$' => {
                    token_started = true;
                    has_variable_expansion = true;
                    if chars.peek() == Some(&'(') {
                        bail!("Command substitution with '$()' is not supported");
                    }
                    current.push(ch);
                }
                '(' | ')' => {
                    token_started = true;
                    has_parentheses = true;
                    current.push(ch);
                }
                '*' | '?' | '[' => {
                    token_started = true;
                    has_unquoted_glob = true;
                    current.push(ch);
                }
                '|' => {
                    flush(
                        &mut items,
                        &mut current,
                        &mut token_started,
                        &mut has_unquoted_glob,
                        &mut has_variable_expansion,
                        &mut has_parentheses,
                    );
                    if chars.peek() == Some(&'|') {
                        chars.next();
                        items.push(LexItem::Operator(Operator::OrIf));
                    } else {
                        items.push(LexItem::Operator(Operator::Pipe));
                    }
                }
                '&' => {
                    flush(
                        &mut items,
                        &mut current,
                        &mut token_started,
                        &mut has_unquoted_glob,
                        &mut has_variable_expansion,
                        &mut has_parentheses,
                    );
                    if chars.peek() == Some(&'&') {
                        chars.next();
                        items.push(LexItem::Operator(Operator::AndIf));
                    } else {
                        bail!("Background execution with '&' is not supported");
                    }
                }
                ';' => {
                    flush(
                        &mut items,
                        &mut current,
                        &mut token_started,
                        &mut has_unquoted_glob,
                        &mut has_variable_expansion,
                        &mut has_parentheses,
                    );
                    items.push(LexItem::Operator(Operator::Semicolon));
                }
                '<' => {
                    flush(
                        &mut items,
                        &mut current,
                        &mut token_started,
                        &mut has_unquoted_glob,
                        &mut has_variable_expansion,
                        &mut has_parentheses,
                    );
                    match chars.peek() {
                        Some('<') => {
                            chars.next();
                            if chars.peek() == Some(&'<') {
                                bail!("Here-strings are not supported");
                            }
                            bail!("Heredocs are not supported");
                        }
                        Some('(') => bail!("Process substitution is not supported"),
                        _ => items.push(LexItem::Operator(Operator::RedirectIn)),
                    }
                }
                '>' => {
                    flush(
                        &mut items,
                        &mut current,
                        &mut token_started,
                        &mut has_unquoted_glob,
                        &mut has_variable_expansion,
                        &mut has_parentheses,
                    );
                    match chars.peek() {
                        Some('>') => {
                            chars.next();
                            items.push(LexItem::Operator(Operator::RedirectAppend));
                        }
                        Some('(') => bail!("Process substitution is not supported"),
                        _ => items.push(LexItem::Operator(Operator::RedirectOut)),
                    }
                }
                _ => {
                    token_started = true;
                    current.push(ch);
                }
            },
            State::SingleQuoted => {
                if ch == '\'' {
                    state = State::Normal;
                } else {
                    current.push(ch);
                }
            }
            State::DoubleQuoted => match ch {
                '"' => state = State::Normal,
                '\\' => {
                    let Some(next) = chars.next() else {
                        bail!("Shell command ends with a dangling escape");
                    };
                    current.push(next);
                }
                '`' => bail!("Command substitution with backticks is not supported"),
                '$' => {
                    has_variable_expansion = true;
                    if chars.peek() == Some(&'(') {
                        bail!("Command substitution with '$()' is not supported");
                    }
                    current.push(ch);
                }
                _ => current.push(ch),
            },
        }
    }

    match state {
        State::Normal => {}
        State::SingleQuoted | State::DoubleQuoted => {
            bail!("Shell command contains unbalanced quotes");
        }
    }

    flush(
        &mut items,
        &mut current,
        &mut token_started,
        &mut has_unquoted_glob,
        &mut has_variable_expansion,
        &mut has_parentheses,
    );

    Ok(items)
}

async fn execute_request(request: &NormalizedRequest) -> Result<ExecutionOutcome> {
    let mut command = match &request.execution {
        ExecutionRequest::Program { program, args } => {
            let mut command = Command::new(program);
            command.args(args);
            command
        }
        ExecutionRequest::Shell {
            shell_path,
            command,
        } => {
            let mut shell = Command::new(shell_path);
            shell.arg("-lc").arg(command);
            shell
        }
    };

    command
        .current_dir(&request.cwd_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }

    let mut child = command.spawn().context("Failed to spawn command")?;
    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;
    let pid = child.id();

    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(read_output(stdout, OutputStream::Stdout, tx.clone()));
    tokio::spawn(read_output(stderr, OutputStream::Stderr, tx));

    let start = Instant::now();
    let timeout = tokio::time::sleep(Duration::from_millis(request.timeout_ms));
    tokio::pin!(timeout);

    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut total_chars = 0usize;
    let mut timed_out = false;
    let mut output_truncated = false;
    let mut exit_code = None;
    let mut child_done = false;
    let mut readers_done = false;

    loop {
        tokio::select! {
            maybe_chunk = rx.recv(), if !readers_done => {
                match maybe_chunk {
                    Some((stream, chunk)) => {
                        if output_truncated {
                            continue;
                        }

                        let remaining = request.max_output.saturating_sub(total_chars);
                        if remaining == 0 {
                            output_truncated = true;
                            if let Some(id) = pid {
                                kill_process_group(id);
                            }
                            exit_code = child.wait().await.ok().and_then(|status| status.code());
                            child_done = true;
                            continue;
                        }

                        let clipped = truncate_to_chars(&chunk, remaining);
                        total_chars += clipped.chars().count();
                        match stream {
                            OutputStream::Stdout => stdout_text.push_str(&clipped),
                            OutputStream::Stderr => stderr_text.push_str(&clipped),
                        }

                        if clipped.len() < chunk.len() || total_chars >= request.max_output {
                            output_truncated = true;
                            if let Some(id) = pid {
                                kill_process_group(id);
                            }
                            exit_code = child.wait().await.ok().and_then(|status| status.code());
                            child_done = true;
                        }
                    }
                    None => readers_done = true,
                }
            }
            status = child.wait(), if !child_done => {
                exit_code = status.context("Failed to wait for command")?.code();
                child_done = true;
            }
            _ = &mut timeout, if !child_done => {
                timed_out = true;
                if let Some(id) = pid {
                    kill_process_group(id);
                }
                exit_code = child.wait().await.ok().and_then(|status| status.code());
                child_done = true;
            }
        }

        if child_done && readers_done {
            break;
        }
    }

    Ok(ExecutionOutcome {
        exit_code,
        timed_out,
        output_truncated,
        stdout: stdout_text,
        stderr: stderr_text,
        duration_ms: start.elapsed().as_millis(),
    })
}

async fn read_output(
    mut stream: impl AsyncReadExt + Unpin,
    which: OutputStream,
    tx: mpsc::UnboundedSender<(OutputStream, String)>,
) {
    let mut buffer = vec![0u8; 4096];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                if tx.send((which, text)).is_err() {
                    break;
                }
            }
            Err(err) => {
                let _ = tx.send((
                    OutputStream::Stderr,
                    format!("[zo failed to read command output: {}]\n", err),
                ));
                break;
            }
        }
    }
}

fn truncate_to_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

fn read_confirmation_response() -> Result<String> {
    #[cfg(unix)]
    {
        if let Ok(tty) = File::open("/dev/tty") {
            let mut reader = io::BufReader::new(tty);
            let mut response = String::new();
            reader.read_line(&mut response)?;
            return Ok(response);
        }
    }

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(response)
}

fn is_confirmation_approved(response: &str) -> bool {
    let response = response.trim();
    response.is_empty()
        || response.eq_ignore_ascii_case("y")
        || response.eq_ignore_ascii_case("yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher_exact(value: &str) -> ShellArgMatcher {
        ShellArgMatcher {
            exact: Some(value.to_string()),
            glob: None,
            regex: None,
        }
    }

    fn matcher_regex(value: &str) -> ShellArgMatcher {
        ShellArgMatcher {
            exact: None,
            glob: None,
            regex: Some(value.to_string()),
        }
    }

    #[test]
    fn test_confirmation_defaults_to_yes() {
        assert!(is_confirmation_approved(""));
        assert!(is_confirmation_approved("y"));
        assert!(is_confirmation_approved("yes"));
        assert!(is_confirmation_approved(" Y "));
        assert!(!is_confirmation_approved("n"));
        assert!(!is_confirmation_approved("no"));
        assert!(!is_confirmation_approved("anything else"));
    }

    fn test_runtime() -> ShellRuntime {
        ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Ask,
                allowed_shells: vec!["/bin/sh".to_string(), "/bin/bash".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: false,
                    program: Some("git".to_string()),
                    args: vec![matcher_exact("status"), matcher_exact("--porcelain")],
                    args_prefix: Vec::new(),
                    command_glob: None,
                    command_regex: None,
                }],
                policy_sets: vec![ShellPolicySet {
                    name: "makefile".to_string(),
                    entries: vec![ShellPolicyEntry {
                        action: ShellPolicyAction::Allow,
                        terminal: false,
                        program: None,
                        args: Vec::new(),
                        args_prefix: Vec::new(),
                        command_glob: Some("make *".to_string()),
                        command_regex: None,
                    }],
                }],
            },
            &["makefile".to_string()],
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn test_parse_shell_command_pipeline() {
        let parsed = parse_shell_command("git status --short | head -n 5").unwrap();
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].program, "git");
        assert_eq!(parsed.segments[1].program, "head");
        assert!(parsed.gate_reasons.is_empty());
    }

    #[test]
    fn test_parse_shell_command_rejects_command_substitution() {
        let result = parse_shell_command("echo $(pwd)");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_shell_command_marks_redirection_as_gated() {
        let parsed = parse_shell_command("git status > out.txt").unwrap();
        assert!(
            parsed
                .gate_reasons
                .iter()
                .any(|reason| reason.contains("redirection"))
        );
    }

    #[test]
    fn test_match_program_entry_requires_exact_arity() {
        let entry = ShellPolicyEntry {
            action: ShellPolicyAction::Allow,
            terminal: false,
            program: Some("head".to_string()),
            args: vec![matcher_exact("-n")],
            args_prefix: Vec::new(),
            command_glob: None,
            command_regex: None,
        };
        let segment = NormalizedSegment {
            program: "head".to_string(),
            args: vec!["-n".to_string(), "5".to_string()],
        };
        assert!(!match_program_entry(&entry, &segment));
    }

    #[test]
    fn test_match_program_entry_accepts_args_prefix_with_trailing_args() {
        let entry = ShellPolicyEntry {
            action: ShellPolicyAction::Allow,
            terminal: false,
            program: Some("git".to_string()),
            args: Vec::new(),
            args_prefix: vec![matcher_exact("status")],
            command_glob: None,
            command_regex: None,
        };
        let segment = NormalizedSegment {
            program: "git".to_string(),
            args: vec!["status".to_string(), "--porcelain".to_string()],
        };
        assert!(match_program_entry(&entry, &segment));
    }

    #[test]
    fn test_arg_regex_requires_full_string_match() {
        let entry = ShellPolicyEntry {
            action: ShellPolicyAction::Allow,
            terminal: false,
            program: Some("git".to_string()),
            args: vec![matcher_regex("status")],
            args_prefix: Vec::new(),
            command_glob: None,
            command_regex: None,
        };
        let segment = NormalizedSegment {
            program: "git".to_string(),
            args: vec!["statusx".to_string()],
        };
        assert!(!match_program_entry(&entry, &segment));
    }

    #[test]
    fn test_policy_allow_for_normalized_git_status() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Allow);
    }

    #[test]
    fn test_policy_allow_for_args_prefix_with_trailing_args() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Ask,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: false,
                    program: Some("git".to_string()),
                    args: Vec::new(),
                    args_prefix: vec![matcher_exact("status")],
                    command_glob: None,
                    command_regex: None,
                }],
                policy_sets: Vec::new(),
            },
            &[],
            false,
            false,
        )
        .unwrap();

        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Allow);
    }

    #[test]
    fn test_command_regex_requires_full_string_match() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Deny,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: false,
                    program: None,
                    args: Vec::new(),
                    args_prefix: Vec::new(),
                    command_glob: None,
                    command_regex: Some("git status".to_string()),
                }],
                policy_sets: Vec::new(),
            },
            &[],
            false,
            false,
        )
        .unwrap();

        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Deny);
    }

    #[test]
    fn test_policy_chained_command_does_not_auto_allow() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_shell_request(
                RunShellCommandParams {
                    command: "git status --porcelain && curl https://example.com".to_string(),
                    cwd: None,
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        let decision = runtime.evaluate_policy(&request);
        let action = if request.requires_approval && decision.action == ShellPolicyAction::Allow {
            ShellPolicyAction::Ask
        } else {
            decision.action
        };
        assert_eq!(action, ShellPolicyAction::Ask);
    }

    #[test]
    fn test_policy_set_command_glob_matches() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_shell_request(
                RunShellCommandParams {
                    command: "make test".to_string(),
                    cwd: None,
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Allow);
    }

    #[test]
    fn test_policy_later_program_rule_overrides_earlier_command_glob() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Ask,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: false,
                    program: None,
                    args: Vec::new(),
                    args_prefix: Vec::new(),
                    command_glob: Some("git status --porcelain".to_string()),
                    command_regex: None,
                }],
                policy_sets: vec![ShellPolicySet {
                    name: "git".to_string(),
                    entries: vec![ShellPolicyEntry {
                        action: ShellPolicyAction::Deny,
                        terminal: false,
                        program: Some("git".to_string()),
                        args: vec![matcher_exact("status"), matcher_exact("--porcelain")],
                        args_prefix: Vec::new(),
                        command_glob: None,
                        command_regex: None,
                    }],
                }],
            },
            &["git".to_string()],
            false,
            false,
        )
        .unwrap();

        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Deny);
    }

    #[test]
    fn test_policy_later_command_glob_overrides_earlier_program_rule() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Ask,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: false,
                    program: Some("git".to_string()),
                    args: vec![matcher_exact("status"), matcher_exact("--porcelain")],
                    args_prefix: Vec::new(),
                    command_glob: None,
                    command_regex: None,
                }],
                policy_sets: vec![ShellPolicySet {
                    name: "git".to_string(),
                    entries: vec![ShellPolicyEntry {
                        action: ShellPolicyAction::Deny,
                        terminal: false,
                        program: None,
                        args: Vec::new(),
                        args_prefix: Vec::new(),
                        command_glob: Some("git status --porcelain".to_string()),
                        command_regex: None,
                    }],
                }],
            },
            &["git".to_string()],
            false,
            false,
        )
        .unwrap();

        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Deny);
    }

    #[test]
    fn test_terminal_program_rule_locks_only_matching_pipeline_segment() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Deny,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Ask,
                    terminal: true,
                    program: Some("git".to_string()),
                    args: vec![matcher_exact("status"), matcher_exact("--porcelain")],
                    args_prefix: Vec::new(),
                    command_glob: None,
                    command_regex: None,
                }],
                policy_sets: vec![ShellPolicySet {
                    name: "pipeline".to_string(),
                    entries: vec![ShellPolicyEntry {
                        action: ShellPolicyAction::Allow,
                        terminal: false,
                        program: None,
                        args: Vec::new(),
                        args_prefix: Vec::new(),
                        command_glob: Some("git status --porcelain | head -n 5".to_string()),
                        command_regex: None,
                    }],
                }],
            },
            &["pipeline".to_string()],
            false,
            false,
        )
        .unwrap();

        let request = runtime
            .normalize_shell_request(
                RunShellCommandParams {
                    command: "git status --porcelain | head -n 5".to_string(),
                    cwd: None,
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Ask);
    }

    #[test]
    fn test_terminal_command_glob_locks_entire_pipeline() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Deny,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: true,
                    program: None,
                    args: Vec::new(),
                    args_prefix: Vec::new(),
                    command_glob: Some("git status --porcelain | head -n 5".to_string()),
                    command_regex: None,
                }],
                policy_sets: vec![ShellPolicySet {
                    name: "git".to_string(),
                    entries: vec![ShellPolicyEntry {
                        action: ShellPolicyAction::Deny,
                        terminal: false,
                        program: Some("git".to_string()),
                        args: vec![matcher_exact("status"), matcher_exact("--porcelain")],
                        args_prefix: Vec::new(),
                        command_glob: None,
                        command_regex: None,
                    }],
                }],
            },
            &["git".to_string()],
            false,
            false,
        )
        .unwrap();

        let request = runtime
            .normalize_shell_request(
                RunShellCommandParams {
                    command: "git status --porcelain | head -n 5".to_string(),
                    cwd: None,
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let decision = runtime.evaluate_policy(&request);
        assert_eq!(decision.action, ShellPolicyAction::Allow);
    }

    #[tokio::test]
    async fn test_non_interactive_denies_approval_required_command() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Ask,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: Vec::new(),
                policy_sets: Vec::new(),
            },
            &[],
            true,
            false,
        )
        .unwrap();

        let mut retry_guard = HashSet::new();
        let response = runtime
            .execute_shell_command(
                RunShellCommandParams {
                    command: "echo hello".to_string(),
                    cwd: None,
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
                &InlineColors::default(),
                &mut retry_guard,
            )
            .await
            .unwrap();

        assert!(response.contains("\"kind\": \"approval_denied\""));
        assert!(response.contains("--non-interactive"));
    }

    #[tokio::test]
    async fn test_terminal_allow_still_requires_approval() {
        let runtime = ShellRuntime::new(
            &ShellConfig {
                default_action: ShellPolicyAction::Deny,
                allowed_shells: vec!["/bin/sh".to_string()],
                always_on: vec![ShellPolicyEntry {
                    action: ShellPolicyAction::Allow,
                    terminal: true,
                    program: None,
                    args: Vec::new(),
                    args_prefix: Vec::new(),
                    command_glob: Some("echo hello && printf world".to_string()),
                    command_regex: None,
                }],
                policy_sets: Vec::new(),
            },
            &[],
            true,
            false,
        )
        .unwrap();

        let mut retry_guard = HashSet::new();
        let response = runtime
            .execute_shell_command(
                RunShellCommandParams {
                    command: "echo hello && printf world".to_string(),
                    cwd: None,
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
                &InlineColors::default(),
                &mut retry_guard,
            )
            .await
            .unwrap();

        assert!(response.contains("\"kind\": \"approval_denied\""));
        assert!(response.contains("resolved to shell action 'allow'"));
    }

    #[tokio::test]
    async fn test_execute_program_captures_stdout() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "printf".to_string(),
                    args: vec!["hello".to_string()],
                    cwd: None,
                    timeout_ms: Some(5_000),
                    max_output: Some(100),
                },
                false,
            )
            .unwrap();
        let outcome = execute_request(&request).await.unwrap();
        assert_eq!(outcome.stdout, "hello");
        assert!(!outcome.timed_out);
    }

    #[tokio::test]
    async fn test_execute_program_truncates_output() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "printf".to_string(),
                    args: vec!["abcdefghijklmnopqrstuvwxyz".to_string()],
                    cwd: None,
                    timeout_ms: Some(5_000),
                    max_output: Some(5),
                },
                false,
            )
            .unwrap();
        let outcome = execute_request(&request).await.unwrap();
        assert_eq!(outcome.stdout, "abcde");
        assert!(outcome.output_truncated);
    }

    #[test]
    fn test_suggested_rule_is_valid_toml_for_simple_command() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let rule = suggested_rule(&request).unwrap();
        let config = format!(
            "[shell]\n[[shell.always_on]]\naction = \"allow\"\n{}\n",
            rule
        );
        let parsed: crate::config::Config = toml::from_str(&config).unwrap();

        assert_eq!(parsed.shell.always_on[0].program.as_deref(), Some("git"));
        assert_eq!(
            parsed.shell.always_on[0].args,
            vec![matcher_exact("status"), matcher_exact("--porcelain")]
        );
    }

    #[test]
    fn test_suggested_rule_escapes_special_characters_for_toml() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "printf".to_string(),
                    args: vec!["can't\\\"stop now".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let rule = suggested_rule(&request).unwrap();
        let config = format!(
            "[shell]\n[[shell.always_on]]\naction = \"allow\"\n{}\n",
            rule
        );
        let parsed: crate::config::Config = toml::from_str(&config).unwrap();

        assert_eq!(parsed.shell.always_on[0].program.as_deref(), Some("printf"));
        assert_eq!(
            parsed.shell.always_on[0].args,
            vec![matcher_exact("can't\\\"stop now")]
        );
    }

    #[test]
    fn test_suggested_family_rule_is_valid_toml_for_subcommand_prefix() {
        let runtime = test_runtime();
        let request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();

        let rule = suggested_family_rule(&request).unwrap();
        let config = format!(
            "[shell]\n[[shell.always_on]]\naction = \"allow\"\n{}\n",
            rule
        );
        let parsed: crate::config::Config = toml::from_str(&config).unwrap();

        assert_eq!(parsed.shell.always_on[0].program.as_deref(), Some("git"));
        assert_eq!(
            parsed.shell.always_on[0].args_prefix,
            vec![matcher_exact("status")]
        );
        assert!(parsed.shell.always_on[0].args.is_empty());
    }

    #[test]
    fn test_build_approval_prompt_compact_program_request() {
        let runtime = test_runtime();
        let mut request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "printf".to_string(),
                    args: vec!["hello".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        request.resolved_programs.clear();

        let prompt = build_approval_prompt(&request, "Needs approval.", false);

        assert_eq!(prompt.command, "printf hello");
        assert_eq!(
            prompt.metadata_line,
            "timeout: 30000 ms · output: 24000 chars"
        );
        assert!(prompt.detail_lines.is_empty());
    }

    #[test]
    fn test_build_approval_prompt_includes_shell_and_nondefault_cwd() {
        let runtime = test_runtime();
        let mut request = runtime
            .normalize_shell_request(
                RunShellCommandParams {
                    command: "make test".to_string(),
                    cwd: Some("src".to_string()),
                    shell: Some("/bin/sh".to_string()),
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        request.resolved_programs.clear();

        let prompt = build_approval_prompt(&request, "Needs approval.", false);

        assert_eq!(
            prompt.metadata_line,
            "shell: /bin/sh · cwd: src · timeout: 30000 ms · output: 24000 chars"
        );
        assert!(prompt.detail_lines.is_empty());
    }

    #[test]
    fn test_build_approval_prompt_verbose_includes_exact_and_family_rules() {
        let runtime = test_runtime();
        let mut request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "git".to_string(),
                    args: vec!["status".to_string(), "--porcelain".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        request.resolved_programs.clear();

        let prompt = build_approval_prompt(&request, "Policy requested approval.", true);

        assert_eq!(prompt.detail_lines[0], "Reason: Policy requested approval.");
        assert_eq!(prompt.detail_lines[1], "Exact durable rule:");
        assert_eq!(prompt.detail_lines[2], "  program = \"git\"");
        assert_eq!(
            prompt.detail_lines[3],
            "  args = [{ exact = \"status\" }, { exact = \"--porcelain\" }]"
        );
        assert_eq!(prompt.detail_lines[4], "Family durable rule (broader):");
        assert_eq!(prompt.detail_lines[5], "  program = \"git\"");
        assert_eq!(
            prompt.detail_lines[6],
            "  args_prefix = [{ exact = \"status\" }]"
        );
    }

    #[test]
    fn test_build_approval_prompt_verbose_includes_gates_executables_and_exact_rule() {
        let runtime = test_runtime();
        let mut request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "printf".to_string(),
                    args: vec!["-n".to_string(), "hello".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        request.requires_approval = true;
        request.gate_reasons = vec![
            "Uses '&&' command chaining, which always requires approval.".to_string(),
            "Touches hidden path '.env'; hidden paths require approval without --hidden."
                .to_string(),
        ];
        request.resolved_programs = vec!["/usr/bin/printf".to_string()];

        let prompt = build_approval_prompt(&request, "Policy requested approval.", true);

        assert_eq!(prompt.detail_lines[0], "Reason: Policy requested approval.");
        assert_eq!(
            prompt.detail_lines[1],
            "Gates: Uses '&&' command chaining, which always requires approval.; Touches hidden path '.env'; hidden paths require approval without --hidden."
        );
        assert_eq!(prompt.detail_lines[2], "Executables: /usr/bin/printf");
        assert_eq!(prompt.detail_lines[3], "Exact durable rule:");
        assert_eq!(prompt.detail_lines[4], "  program = \"printf\"");
        assert_eq!(
            prompt.detail_lines[5],
            "  args = [{ exact = \"-n\" }, { exact = \"hello\" }]"
        );
        assert_eq!(prompt.detail_lines.len(), 6);
    }

    #[test]
    fn test_build_approval_prompt_omits_family_rule_when_first_arg_is_flag() {
        let runtime = test_runtime();
        let mut request = runtime
            .normalize_program_request(
                RunProgramParams {
                    program: "printf".to_string(),
                    args: vec!["-n".to_string(), "hello".to_string()],
                    cwd: None,
                    timeout_ms: None,
                    max_output: None,
                },
                false,
            )
            .unwrap();
        request.resolved_programs.clear();

        let prompt = build_approval_prompt(&request, "Policy requested approval.", true);

        assert_eq!(prompt.detail_lines[0], "Reason: Policy requested approval.");
        assert_eq!(prompt.detail_lines[1], "Exact durable rule:");
        assert!(
            !prompt
                .detail_lines
                .iter()
                .any(|line| line == "Family durable rule (broader):")
        );
    }
}
