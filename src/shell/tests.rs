use super::*;

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

fn runtime_with_rules(
    default_action: ShellPolicyAction,
    rules: &[&str],
    non_interactive: bool,
) -> ShellRuntime {
    ShellRuntime {
        default_action,
        allowed_shells: vec!["/bin/sh".to_string(), "/bin/bash".to_string()],
        non_interactive,
        show_verbose_approval_details: false,
        entries: rules
            .iter()
            .map(|rule| parse_policy_rule(rule, 1, Path::new("test-policy")).unwrap())
            .collect(),
        active_policy_names: Vec::new(),
    }
}

fn test_runtime() -> ShellRuntime {
    runtime_with_rules(
        ShellPolicyAction::Ask,
        &["allow git status --porcelain", "allow make test"],
        false,
    )
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

fn temp_policy_config_dir(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("zo-policy-test-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("policies")).unwrap();
    path
}

#[test]
fn test_policy_file_default_loads_and_runs_inline_tests() {
    let config_dir = temp_policy_config_dir("default-loads");
    fs::write(
        config_dir.join("policies").join("default"),
        r#"
allow git status
deny gh auth **
allow gh pr view /\d+/
#TEST allow git status
#TEST deny gh auth login
#TEST allow gh pr view 100
#TEST default gh pr view abc
"#,
    )
    .unwrap();
    let config = ShellConfig::default();
    let registry = load_shell_policy_registry(&config, &config_dir).unwrap();
    let runtime =
        ShellRuntime::new_with_policy_registry(&config, &registry, &[], false, false).unwrap();

    assert_eq!(runtime.active_policy_names(), &["default".to_string()]);
    let allowed = runtime
        .normalize_program_request(
            RunProgramParams {
                program: "gh".to_string(),
                args: vec!["pr".to_string(), "view".to_string(), "100".to_string()],
                cwd: None,
                timeout_ms: None,
                max_output: None,
            },
            false,
        )
        .unwrap();
    assert_eq!(
        runtime.evaluate_policy(&allowed).action,
        ShellPolicyAction::Allow
    );
}

#[test]
fn test_policy_inline_default_expectation_ignores_configured_default_action() {
    let config_dir = temp_policy_config_dir("test-default-expectation");
    fs::write(
        config_dir.join("policies").join("default"),
        "allow git status\n#TEST default ps aux\n",
    )
    .unwrap();
    let config = ShellConfig {
        default_action: ShellPolicyAction::Deny,
        ..ShellConfig::default()
    };

    assert!(load_shell_policy_registry(&config, &config_dir).is_ok());
}

#[test]
fn test_policy_inline_default_expectation_fails_when_rule_matches() {
    let config_dir = temp_policy_config_dir("test-default-expectation-fails");
    fs::write(
        config_dir.join("policies").join("default"),
        "allow git status\n#TEST default git status\n",
    )
    .unwrap();
    let result = load_shell_policy_registry(&ShellConfig::default(), &config_dir);

    assert!(result.is_err());
    let error = result.unwrap_err().to_string();
    assert!(error.contains("expected 'default'"));
    assert!(error.contains("got 'allow'"));
}

#[test]
fn test_named_policy_does_not_include_default_policy() {
    let config_dir = temp_policy_config_dir("named-excludes-default");
    fs::write(config_dir.join("policies").join("default"), "deny ps **\n").unwrap();
    fs::write(config_dir.join("policies").join("coding"), "allow ps aux\n").unwrap();
    let config = ShellConfig::default();
    let registry = load_shell_policy_registry(&config, &config_dir).unwrap();
    let named = ShellRuntime::new_with_policy_registry(
        &config,
        &registry,
        &["coding".to_string()],
        false,
        false,
    )
    .unwrap();

    let request = named
        .normalize_program_request(
            RunProgramParams {
                program: "ps".to_string(),
                args: vec!["aux".to_string()],
                cwd: None,
                timeout_ms: None,
                max_output: None,
            },
            false,
        )
        .unwrap();
    assert_eq!(
        named.evaluate_policy(&request).action,
        ShellPolicyAction::Allow
    );
}

#[test]
fn test_policy_file_names_are_case_insensitive_unique() {
    let config_dir = temp_policy_config_dir("duplicate-names");
    fs::write(
        config_dir.join("policies").join("coding"),
        "allow git status\n",
    )
    .unwrap();
    fs::write(config_dir.join("policies").join("Coding"), "allow ps aux\n").unwrap();
    let policy_count = fs::read_dir(config_dir.join("policies"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .ok()
                .and_then(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .map(|name| name.eq_ignore_ascii_case("coding"))
                })
                .unwrap_or(false)
        })
        .count();
    if policy_count < 2 {
        return;
    }
    let result = load_shell_policy_registry(&ShellConfig::default(), &config_dir);

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Duplicate shell policy name")
    );
}

#[test]
fn test_policy_file_rejects_rules_after_inline_tests() {
    let config_dir = temp_policy_config_dir("rule-after-test");
    fs::write(
        config_dir.join("policies").join("default"),
        "#TEST ask ps aux\nallow git status\n",
    )
    .unwrap();
    let result = load_shell_policy_registry(&ShellConfig::default(), &config_dir);

    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("rules are not allowed after #TEST")
    );
}

#[test]
fn test_policy_inline_tests_compare_effective_action_after_gates() {
    let config_dir = temp_policy_config_dir("test-gates");
    fs::write(
        config_dir.join("policies").join("default"),
        "allow echo **\n#TEST ask echo *\n",
    )
    .unwrap();

    assert!(load_shell_policy_registry(&ShellConfig::default(), &config_dir).is_ok());
}

#[test]
fn test_dsl_rule_exact_args_require_exact_arity() {
    let rule = parse_policy_rule("allow head -n", 1, Path::new("test-policy")).unwrap();
    let segment = NormalizedSegment {
        program: "head".to_string(),
        args: vec!["-n".to_string(), "5".to_string()],
    };
    assert!(!match_dsl_rule(&rule, &segment));
}

#[test]
fn test_dsl_rule_zero_or_more_accepts_trailing_args() {
    let rule = parse_policy_rule("allow git status **", 1, Path::new("test-policy")).unwrap();
    let segment = NormalizedSegment {
        program: "git".to_string(),
        args: vec!["status".to_string(), "--porcelain".to_string()],
    };
    assert!(match_dsl_rule(&rule, &segment));
}

#[test]
fn test_dsl_regex_requires_full_string_match() {
    let rule = parse_policy_rule("allow git /status/", 1, Path::new("test-policy")).unwrap();
    let segment = NormalizedSegment {
        program: "git".to_string(),
        args: vec!["statusx".to_string()],
    };
    assert!(!match_dsl_rule(&rule, &segment));
}

#[test]
fn test_dsl_regex_alternation_requires_full_string_match() {
    let rule = parse_policy_rule("allow npm /run|build/", 1, Path::new("test-policy")).unwrap();

    assert!(match_dsl_rule(
        &rule,
        &NormalizedSegment {
            program: "npm".to_string(),
            args: vec!["run".to_string()],
        }
    ));
    assert!(match_dsl_rule(
        &rule,
        &NormalizedSegment {
            program: "npm".to_string(),
            args: vec!["build".to_string()],
        }
    ));
    assert!(!match_dsl_rule(
        &rule,
        &NormalizedSegment {
            program: "npm".to_string(),
            args: vec!["build-dangerously".to_string()],
        }
    ));
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
fn test_policy_allow_for_dsl_zero_or_more_with_trailing_args() {
    let runtime = runtime_with_rules(ShellPolicyAction::Ask, &["allow git status **"], false);

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
fn test_policy_dsl_regex_requires_full_string_match() {
    let runtime = runtime_with_rules(ShellPolicyAction::Deny, &["allow git /status/"], false);

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
fn test_policy_dsl_regex_alternation_requires_full_argument_match() {
    let runtime = runtime_with_rules(ShellPolicyAction::Deny, &["allow npm /run|build/"], false);

    let build_request = runtime
        .normalize_program_request(
            RunProgramParams {
                program: "npm".to_string(),
                args: vec!["build".to_string()],
                cwd: None,
                timeout_ms: None,
                max_output: None,
            },
            false,
        )
        .unwrap();
    let dangerous_request = runtime
        .normalize_program_request(
            RunProgramParams {
                program: "npm".to_string(),
                args: vec!["build-dangerously".to_string()],
                cwd: None,
                timeout_ms: None,
                max_output: None,
            },
            false,
        )
        .unwrap();

    assert_eq!(
        runtime.evaluate_policy(&build_request).action,
        ShellPolicyAction::Allow
    );
    assert_eq!(
        runtime.evaluate_policy(&dangerous_request).action,
        ShellPolicyAction::Deny
    );
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
fn test_policy_dsl_rule_matches_shell_segment() {
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
fn test_policy_later_dsl_rule_overrides_earlier_rule() {
    let runtime = runtime_with_rules(
        ShellPolicyAction::Ask,
        &[
            "allow git status --porcelain",
            "deny git status --porcelain",
        ],
        false,
    );

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
fn test_policy_later_dsl_allow_overrides_earlier_deny() {
    let runtime = runtime_with_rules(
        ShellPolicyAction::Ask,
        &[
            "deny git status --porcelain",
            "allow git status --porcelain",
        ],
        false,
    );

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

#[tokio::test]
async fn test_non_interactive_denies_approval_required_command() {
    let runtime = runtime_with_rules(ShellPolicyAction::Ask, &[], true);

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
async fn test_allowed_gated_command_still_requires_approval() {
    let runtime = runtime_with_rules(ShellPolicyAction::Allow, &[], true);

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
    assert!(response.contains("--non-interactive"));
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
fn test_suggested_rule_is_valid_policy_dsl_for_simple_command() {
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
    let parsed = parse_policy_rule(&rule, 1, Path::new("test-policy")).unwrap();

    assert_eq!(rule, "allow git status --porcelain");
    assert!(match_dsl_rule(&parsed, request.segments.first().unwrap()));
}

#[test]
fn test_suggested_rule_escapes_special_characters_for_policy_dsl() {
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
    let parsed = parse_policy_rule(&rule, 1, Path::new("test-policy")).unwrap();

    assert!(match_dsl_rule(&parsed, request.segments.first().unwrap()));
}

#[test]
fn test_suggested_family_rule_is_valid_policy_dsl_for_subcommand_family() {
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
    let parsed = parse_policy_rule(&rule, 1, Path::new("test-policy")).unwrap();

    assert_eq!(rule, "allow git status **");
    assert!(match_dsl_rule(&parsed, request.segments.first().unwrap()));
    assert!(match_dsl_rule(
        &parsed,
        &NormalizedSegment {
            program: "git".to_string(),
            args: vec!["status".to_string()],
        }
    ));
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
    assert_eq!(prompt.detail_lines[2], "  allow git status --porcelain");
    assert_eq!(prompt.detail_lines[3], "Family durable rule (broader):");
    assert_eq!(prompt.detail_lines[4], "  allow git status **");
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
        "Touches hidden path '.env'; hidden paths require approval without --hidden.".to_string(),
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
    assert_eq!(prompt.detail_lines[4], "  allow printf -n hello");
    assert_eq!(prompt.detail_lines.len(), 5);
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
