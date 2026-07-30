use super::*;

pub(super) fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(super) fn basename_for_policy(program: &str) -> String {
    Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| program.to_string())
}

pub(super) fn workspace_root() -> Result<PathBuf> {
    env::current_dir()
        .context("Failed to resolve current working directory")?
        .canonicalize()
        .context("Failed to canonicalize current working directory")
}

pub(super) fn normalize_cwd(
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

pub(super) fn normalize_token_for_display(
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

pub(super) enum ExplicitPath {
    InWorkspace {
        display: String,
        touches_hidden: bool,
    },
    External(String),
}

pub(super) fn normalize_explicit_path(token: &str, cwd_path: &Path) -> Option<ExplicitPath> {
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

pub(super) fn looks_like_explicit_path(token: &str) -> bool {
    token == "."
        || token == ".."
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.starts_with('/')
        || token.contains('/')
}

pub(super) fn relative_display_path(path: &Path, root: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("Path not under workspace root: {}", path.display()))?;
    if relative.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(relative.to_string_lossy().to_string())
    }
}

pub(super) fn has_hidden_component(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

pub(super) fn resolve_executable(program: &str, cwd_path: &Path) -> Option<String> {
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

pub(super) fn join_command_words<I>(words: I) -> String
where
    I: IntoIterator<Item = String>,
{
    words
        .into_iter()
        .map(|word| quote_token(&word))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn quote_token(token: &str) -> String {
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

pub(super) fn quote_policy_token(token: &str) -> String {
    let needs_quotes = token.is_empty()
        || matches!(token, "+" | "*" | "++" | "**")
        || token.starts_with('/')
        || token.starts_with('#')
        || token.chars().any(|ch| {
            ch.is_whitespace() || ch == '"' || ch == '\\' || ch == '#' || ch.is_control()
        });

    if !needs_quotes {
        return token.to_string();
    }

    let mut quoted = String::with_capacity(token.len() + 2);
    quoted.push('"');
    for ch in token.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
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

pub(super) fn dedupe_preserve_order(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

pub(super) fn parse_shell_command(command: &str) -> Result<ParsedShellCommand> {
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

pub(super) fn push_segment(
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

pub(super) fn is_env_assignment(token: &str) -> bool {
    let Some((name, _value)) = token.split_once('=') else {
        return false;
    };
    is_valid_env_name(name)
}

pub(super) fn lex_shell(command: &str) -> Result<Vec<LexItem>> {
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
