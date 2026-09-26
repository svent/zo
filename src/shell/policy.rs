use super::*;

pub(super) fn rejected_request(
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

pub(super) fn serialize_response(response: &ShellToolResponse) -> Result<String> {
    serde_json::to_string_pretty(response).context("Failed to serialize shell tool response")
}

pub(super) fn action_name(action: ShellPolicyAction) -> &'static str {
    match action {
        ShellPolicyAction::Allow => "allow",
        ShellPolicyAction::Ask => "ask",
        ShellPolicyAction::Deny => "deny",
    }
}

pub(super) fn match_dsl_rule(rule: &ShellPolicyRule, segment: &NormalizedSegment) -> bool {
    if basename_for_policy(&rule.program) != segment.program {
        return false;
    }

    dsl_args_match(&rule.args, &segment.args)
}

pub(super) fn dsl_args_match(patterns: &[ShellDslArgPattern], args: &[String]) -> bool {
    fn matches_from(
        patterns: &[ShellDslArgPattern],
        args: &[String],
        pattern_index: usize,
        arg_index: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, arg_index)) {
            return *result;
        }

        let result = if pattern_index == patterns.len() {
            arg_index == args.len()
        } else {
            match &patterns[pattern_index] {
                ShellDslArgPattern::Exact(expected) => {
                    args.get(arg_index).is_some_and(|arg| arg == expected)
                        && matches_from(patterns, args, pattern_index + 1, arg_index + 1, memo)
                }
                ShellDslArgPattern::Regex { compiled, .. } => {
                    args.get(arg_index)
                        .is_some_and(|arg| compiled.is_match(arg))
                        && matches_from(patterns, args, pattern_index + 1, arg_index + 1, memo)
                }
                ShellDslArgPattern::AnyOne => {
                    arg_index < args.len()
                        && matches_from(patterns, args, pattern_index + 1, arg_index + 1, memo)
                }
                ShellDslArgPattern::OptionalAny => {
                    matches_from(patterns, args, pattern_index + 1, arg_index, memo)
                        || (arg_index < args.len()
                            && matches_from(patterns, args, pattern_index + 1, arg_index + 1, memo))
                }
                ShellDslArgPattern::OneOrMore => (arg_index + 1..=args.len()).any(|next_index| {
                    matches_from(patterns, args, pattern_index + 1, next_index, memo)
                }),
                ShellDslArgPattern::ZeroOrMore => (arg_index..=args.len()).any(|next_index| {
                    matches_from(patterns, args, pattern_index + 1, next_index, memo)
                }),
            }
        };

        memo.insert((pattern_index, arg_index), result);
        result
    }

    matches_from(patterns, args, 0, 0, &mut HashMap::new())
}

pub(super) fn compile_full_match_regex(pattern: &str) -> Result<Regex, regex::Error> {
    Regex::new(&format!(r"\A(?:{})\z", pattern))
}

pub(super) fn dsl_rule_summary(rule: &ShellPolicyRule) -> String {
    if rule.args.is_empty() {
        return format!("program='{}'", basename_for_policy(&rule.program));
    }

    format!(
        "program='{}' args=[{}]",
        basename_for_policy(&rule.program),
        rule.args
            .iter()
            .map(format_dsl_arg_pattern)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(super) fn format_dsl_arg_pattern(pattern: &ShellDslArgPattern) -> String {
    match pattern {
        ShellDslArgPattern::Exact(value) => format!("exact({})", quote_token(value)),
        ShellDslArgPattern::Regex { source, .. } => format!("regex({})", quote_token(source)),
        ShellDslArgPattern::AnyOne => "+".to_string(),
        ShellDslArgPattern::OptionalAny => "*".to_string(),
        ShellDslArgPattern::OneOrMore => "++".to_string(),
        ShellDslArgPattern::ZeroOrMore => "**".to_string(),
    }
}

pub(super) fn build_approval_prompt(
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

pub(super) fn suggested_rules(request: &NormalizedRequest) -> Vec<DurableRuleSuggestion> {
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

pub(super) fn suggested_rule(request: &NormalizedRequest) -> Option<String> {
    if request.segments.len() != 1 {
        return None;
    }

    let segment = request.segments.first()?;
    let args = segment
        .args
        .iter()
        .map(|arg| quote_policy_token(arg))
        .collect::<Vec<_>>();

    Some(exact_rule_for_segment(&segment.program, &args))
}

pub(super) fn suggested_family_rule(request: &NormalizedRequest) -> Option<String> {
    if request.segments.len() != 1 {
        return None;
    }

    let segment = request.segments.first()?;
    let first_arg = segment.args.first()?;
    if first_arg.starts_with('-') {
        return None;
    }

    Some(format!(
        "allow {} {} **",
        quote_policy_token(&segment.program),
        quote_policy_token(first_arg)
    ))
}

pub(super) fn exact_rule_for_segment(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        format!("allow {}", quote_policy_token(program))
    } else {
        format!("allow {} {}", quote_policy_token(program), args.join(" "))
    }
}

pub(super) fn clamp_timeout(value: Option<u64>) -> u64 {
    value.unwrap_or(DEFAULT_TIMEOUT_MS).clamp(1, MAX_TIMEOUT_MS)
}

pub(super) fn clamp_max_output(value: Option<usize>) -> usize {
    value
        .unwrap_or(DEFAULT_MAX_OUTPUT_CHARS)
        .clamp(1, DEFAULT_MAX_OUTPUT_CHARS)
}

pub(super) fn build_fingerprint(
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

impl ShellPolicyRegistry {
    pub(super) fn get(&self, name: &str) -> Option<&ShellPolicy> {
        self.policies.get(&normalize_policy_name(name))
    }
}

pub fn load_shell_policy_registry(
    config: &ShellConfig,
    config_dir: &Path,
) -> Result<ShellPolicyRegistry> {
    let policy_dir = config_dir.join("policies");
    let mut policies = HashMap::new();

    if !policy_dir.exists() {
        return Ok(ShellPolicyRegistry { policies });
    }

    let mut entries = fs::read_dir(&policy_dir)
        .with_context(|| format!("Failed to read policy directory: {}", policy_dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("Failed to read policy directory: {}", policy_dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to inspect policy file: {}", path.display()))?;
        if !file_type.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(policy_name) = file_name.to_str() else {
            bail!(
                "Policy file name is not valid UTF-8: {}",
                path.to_string_lossy()
            );
        };
        if policy_name.starts_with('.') {
            continue;
        }
        validate_policy_name(policy_name)
            .with_context(|| format!("Invalid policy file name '{}'", policy_name))?;

        let normalized = normalize_policy_name(policy_name);
        if policies.contains_key(&normalized) {
            bail!(
                "Duplicate shell policy name '{}'; policy names are case-insensitive",
                policy_name
            );
        }

        let policy = parse_policy_file(policy_name, &path)?;
        policies.insert(normalized, policy);
    }

    let registry = ShellPolicyRegistry { policies };
    validate_policy_tests(config, &registry)?;
    Ok(registry)
}

pub(super) fn validate_policy_tests(
    config: &ShellConfig,
    registry: &ShellPolicyRegistry,
) -> Result<()> {
    for policy in registry.policies.values() {
        if policy.tests.is_empty() {
            continue;
        }

        let runtime = ShellRuntime {
            default_action: config.default_action,
            allowed_shells: config.allowed_shells.clone(),
            non_interactive: false,
            show_verbose_approval_details: false,
            entries: policy.entries.clone(),
            active_policy_names: vec![policy.name.clone()],
        };

        for test in &policy.tests {
            let shell_path = runtime
                .allowed_shells
                .first()
                .cloned()
                .context("No shells configured in shell.allowed_shells")?;
            let request = runtime
                .normalize_shell_request(
                    RunShellCommandParams {
                        command: test.command.clone(),
                        cwd: None,
                        shell: Some(shell_path),
                        timeout_ms: None,
                        max_output: None,
                    },
                    false,
                )
                .with_context(|| {
                    format!(
                        "{}:{}: failed to parse #TEST command for policy '{}'",
                        policy.path.display(),
                        test.line,
                        policy.name
                    )
                })?;

            if let Some(rejection) = &request.rejection {
                bail!(
                    "{}:{}: #TEST command for policy '{}' is unsupported: {}",
                    policy.path.display(),
                    test.line,
                    policy.name,
                    rejection.reason
                );
            }

            let decision = runtime.evaluate_policy(&request);
            match test.expected {
                ShellPolicyTestExpectation::Default => {
                    if !decision.used_default {
                        let actual = effective_action(&request, decision.action);
                        bail!(
                            "{}:{}: #TEST failed for policy '{}': command `{}` expected 'default' but got '{}'. {}",
                            policy.path.display(),
                            test.line,
                            policy.name,
                            test.command,
                            action_name(actual),
                            decision.reason
                        );
                    }
                }
                ShellPolicyTestExpectation::Action(expected) => {
                    let actual = effective_action(&request, decision.action);
                    if actual != expected {
                        bail!(
                            "{}:{}: #TEST failed for policy '{}': command `{}` expected '{}' but got '{}'. {}",
                            policy.path.display(),
                            test.line,
                            policy.name,
                            test.command,
                            action_name(expected),
                            action_name(actual),
                            decision.reason
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

pub(super) fn effective_action(
    request: &NormalizedRequest,
    action: ShellPolicyAction,
) -> ShellPolicyAction {
    if request.requires_approval && action == ShellPolicyAction::Allow {
        ShellPolicyAction::Ask
    } else {
        action
    }
}

pub(super) fn normalize_policy_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

pub(super) fn validate_policy_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("policy name must not be empty");
    };
    if !first.is_ascii_alphanumeric() {
        bail!("policy name must start with an ASCII letter or digit");
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
        bail!("policy name may only contain ASCII letters, digits, '_' and '-'");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DslToken {
    Word(String),
    Regex(String),
}

pub(super) fn parse_policy_file(policy_name: &str, path: &Path) -> Result<ShellPolicy> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read policy file: {}", path.display()))?;
    let mut entries = Vec::new();
    let mut tests = Vec::new();
    let mut saw_test = false;

    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || (trimmed.starts_with('#') && !is_test_line(trimmed)) {
            continue;
        }

        if let Some(test_text) = parse_test_line(trimmed) {
            saw_test = true;
            tests.push(parse_policy_test(test_text, line_number, path)?);
            continue;
        }

        if saw_test {
            bail!(
                "{}:{}: policy rules are not allowed after #TEST lines",
                path.display(),
                line_number
            );
        }

        entries.push(parse_policy_rule(line, line_number, path)?);
    }

    Ok(ShellPolicy {
        name: policy_name.to_string(),
        path: path.to_path_buf(),
        entries,
        tests,
    })
}

pub(super) fn is_test_line(line: &str) -> bool {
    line == "#TEST" || line.starts_with("#TEST ") || line.starts_with("#TEST:")
}

pub(super) fn parse_test_line(line: &str) -> Option<&str> {
    if line == "#TEST" {
        return Some("");
    }
    line.strip_prefix("#TEST:")
        .or_else(|| line.strip_prefix("#TEST "))
        .map(str::trim)
}

pub(super) fn parse_policy_test(
    text: &str,
    line_number: usize,
    path: &Path,
) -> Result<ShellPolicyTest> {
    let trimmed = text.trim();
    let Some(split_at) = trimmed.find(char::is_whitespace) else {
        bail!(
            "{}:{}: #TEST must use `#TEST <allow|ask|deny|default> <command>`",
            path.display(),
            line_number
        );
    };
    let expected = &trimmed[..split_at];
    let command = trimmed[split_at..].trim();
    if command.is_empty() {
        bail!(
            "{}:{}: #TEST must include a command",
            path.display(),
            line_number
        );
    }

    Ok(ShellPolicyTest {
        line: line_number,
        expected: parse_policy_test_expectation(expected).with_context(|| {
            format!(
                "{}:{}: invalid #TEST expected action '{}'",
                path.display(),
                line_number,
                expected
            )
        })?,
        command: command.to_string(),
    })
}

pub(super) fn parse_policy_test_expectation(value: &str) -> Result<ShellPolicyTestExpectation> {
    if value == "default" {
        Ok(ShellPolicyTestExpectation::Default)
    } else {
        parse_policy_action(value).map(ShellPolicyTestExpectation::Action)
    }
}

pub(super) fn parse_policy_rule(
    line: &str,
    line_number: usize,
    path: &Path,
) -> Result<ShellPolicyRule> {
    let tokens = lex_policy_line(line)
        .with_context(|| format!("{}:{}: invalid policy rule", path.display(), line_number))?;
    if tokens.len() < 2 {
        bail!(
            "{}:{}: policy rule must use `<action> <program> <arg-patterns...>`",
            path.display(),
            line_number
        );
    }

    let DslToken::Word(action_text) = &tokens[0] else {
        bail!(
            "{}:{}: policy action must be allow, ask, or deny",
            path.display(),
            line_number
        );
    };
    let action = parse_policy_action(action_text).with_context(|| {
        format!(
            "{}:{}: invalid policy action '{}'",
            path.display(),
            line_number,
            action_text
        )
    })?;

    let DslToken::Word(program) = &tokens[1] else {
        bail!(
            "{}:{}: policy program must be an exact program name",
            path.display(),
            line_number
        );
    };
    if program.trim().is_empty() {
        bail!(
            "{}:{}: policy program must not be empty",
            path.display(),
            line_number
        );
    }

    let args = tokens[2..]
        .iter()
        .map(parse_dsl_arg_pattern)
        .collect::<Result<Vec<_>>>()
        .with_context(|| {
            format!(
                "{}:{}: invalid policy argument pattern",
                path.display(),
                line_number
            )
        })?;

    Ok(ShellPolicyRule {
        action,
        program: program.clone(),
        args,
    })
}

pub(super) fn parse_policy_action(value: &str) -> Result<ShellPolicyAction> {
    match value {
        "allow" => Ok(ShellPolicyAction::Allow),
        "ask" => Ok(ShellPolicyAction::Ask),
        "deny" => Ok(ShellPolicyAction::Deny),
        _ => bail!("expected allow, ask, or deny"),
    }
}

pub(super) fn parse_dsl_arg_pattern(token: &DslToken) -> Result<ShellDslArgPattern> {
    match token {
        DslToken::Regex(pattern) => Ok(ShellDslArgPattern::Regex {
            source: pattern.clone(),
            compiled: compile_full_match_regex(pattern)
                .with_context(|| format!("invalid regex /{}/", pattern))?,
        }),
        DslToken::Word(value) => match value.as_str() {
            "+" => Ok(ShellDslArgPattern::AnyOne),
            "*" => Ok(ShellDslArgPattern::OptionalAny),
            "++" => Ok(ShellDslArgPattern::OneOrMore),
            "**" => Ok(ShellDslArgPattern::ZeroOrMore),
            _ => Ok(ShellDslArgPattern::Exact(value.clone())),
        },
    }
}

pub(super) fn lex_policy_line(line: &str) -> Result<Vec<DslToken>> {
    let mut chars = line.chars().peekable();
    let mut tokens = Vec::new();

    while let Some(ch) = chars.peek().copied() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if ch == '#' {
            break;
        }
        if ch == '"' {
            tokens.push(DslToken::Word(read_quoted_policy_token(&mut chars)?));
        } else if ch == '/' {
            tokens.push(DslToken::Regex(read_regex_policy_token(&mut chars)?));
        } else {
            tokens.push(DslToken::Word(read_bare_policy_token(&mut chars)));
        }
    }

    Ok(tokens)
}

pub(super) fn read_quoted_policy_token<I>(chars: &mut std::iter::Peekable<I>) -> Result<String>
where
    I: Iterator<Item = char>,
{
    let quote = chars.next();
    debug_assert_eq!(quote, Some('"'));
    let mut value = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Ok(value),
            '\\' => {
                let Some(next) = chars.next() else {
                    bail!("quoted string ends with a dangling escape");
                };
                match next {
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    _ => value.push(next),
                }
            }
            _ => value.push(ch),
        }
    }
    bail!("quoted string is missing its closing quote")
}

pub(super) fn read_regex_policy_token<I>(chars: &mut std::iter::Peekable<I>) -> Result<String>
where
    I: Iterator<Item = char>,
{
    let slash = chars.next();
    debug_assert_eq!(slash, Some('/'));
    let mut value = String::new();
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            value.push('\\');
            value.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '/' => {
                if value.is_empty() {
                    bail!("regex pattern must not be empty");
                }
                return Ok(value);
            }
            _ => value.push(ch),
        }
    }
    bail!("regex pattern is missing its closing slash")
}

pub(super) fn read_bare_policy_token<I>(chars: &mut std::iter::Peekable<I>) -> String
where
    I: Iterator<Item = char>,
{
    let mut value = String::new();
    while let Some(ch) = chars.peek().copied() {
        if ch.is_whitespace() || ch == '#' {
            break;
        }
        value.push(ch);
        chars.next();
    }
    value
}
