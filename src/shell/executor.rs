use super::*;

pub(super) async fn execute_request(request: &NormalizedRequest) -> Result<ExecutionOutcome> {
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

pub(super) async fn read_output(
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

pub(super) fn truncate_to_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(unix)]
pub(super) fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(super) fn kill_process_group(_pid: u32) {}

pub(super) fn read_confirmation_response() -> Result<String> {
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

pub(super) fn is_confirmation_approved(response: &str) -> bool {
    let response = response.trim();
    response.is_empty()
        || response.eq_ignore_ascii_case("y")
        || response.eq_ignore_ascii_case("yes")
}
