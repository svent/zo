use anyhow::{Context, Result, bail};
use crossterm::ExecutableCommand;
use crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use openrouter_rs::types::typed_tool::TypedTool;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::{InlineColors, ShellConfig, ShellPolicyAction};

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
    entries: Vec<ShellPolicyRule>,
    active_policy_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ShellPolicyRegistry {
    policies: HashMap<String, ShellPolicy>,
}

#[derive(Debug, Clone)]
struct ShellPolicy {
    name: String,
    path: PathBuf,
    entries: Vec<ShellPolicyRule>,
    tests: Vec<ShellPolicyTest>,
}

#[derive(Debug, Clone)]
struct ShellPolicyRule {
    action: ShellPolicyAction,
    program: String,
    args: Vec<ShellDslArgPattern>,
}

#[derive(Debug, Clone)]
struct ShellPolicyTest {
    line: usize,
    expected: ShellPolicyTestExpectation,
    command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellPolicyTestExpectation {
    Action(ShellPolicyAction),
    Default,
}

#[derive(Debug, Clone)]
enum ShellDslArgPattern {
    Exact(String),
    Regex { source: String, compiled: Regex },
    AnyOne,
    OptionalAny,
    OneOrMore,
    ZeroOrMore,
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
    used_default: bool,
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
    pub fn new_with_policy_registry(
        config: &ShellConfig,
        registry: &ShellPolicyRegistry,
        active_policy_names: &[String],
        non_interactive: bool,
        show_verbose_approval_details: bool,
    ) -> Result<Self> {
        let mut selected_names = Vec::new();
        if active_policy_names.is_empty() {
            if registry.get("default").is_some() {
                selected_names.push("default".to_string());
            }
        } else {
            selected_names.extend(active_policy_names.iter().cloned());
        }

        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let mut active_display_names = Vec::new();
        for name in &selected_names {
            let normalized = normalize_policy_name(name);
            if !seen.insert(normalized.clone()) {
                bail!("--policies contains duplicate policy name '{}'", name);
            }

            let Some(policy) = registry.get(name) else {
                bail!("Unknown shell policy '{}'", name);
            };
            active_display_names.push(policy.name.clone());
            entries.extend(policy.entries.iter().cloned());
        }

        Ok(Self {
            default_action: config.default_action,
            allowed_shells: config.allowed_shells.clone(),
            non_interactive,
            show_verbose_approval_details,
            entries,
            active_policy_names: active_display_names,
        })
    }

    pub fn active_policy_names(&self) -> &[String] {
        &self.active_policy_names
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
        let action = effective_action(&request, policy.action);

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

        for entry in &self.entries {
            for (index, segment) in request.segments.iter().enumerate() {
                let state = &mut segment_states[index];
                if state.locked || !match_dsl_rule(entry, segment) {
                    continue;
                }

                state.action = Some(entry.action);
                state.summary = Some(dsl_rule_summary(entry));
            }
        }

        if request.segments.len() == 1 {
            if let Some(action) = segment_states[0].action {
                return PolicyDecision {
                    action,
                    used_default: false,
                    reason: format!(
                        "Matched policy entry: {}",
                        segment_states[0].summary.as_deref().unwrap_or_default()
                    ),
                };
            }

            return PolicyDecision {
                action: self.default_action,
                used_default: true,
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
            used_default: all_default,
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

mod executor;
mod parser;
mod policy;

use executor::*;
use parser::*;
pub use policy::*;

#[cfg(test)]
mod tests;
