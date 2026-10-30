use crate::output::{print_json, user_println};
use crate::provider::{self, ProviderProfile, ReasoningLaunch};
use crate::signals::{ShutdownListener, ShutdownSignal};
use crate::{auth, profile};
use anyhow::{Context, Result};
use std::io::{self, IsTerminal};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TuiLaunchOutcome {
    Exited(i32),
    Shutdown {
        signal: ShutdownSignal,
        cleanup_error: Option<String>,
    },
}

fn shutdown_outcome(signal: ShutdownSignal, cleanup: Result<()>) -> TuiLaunchOutcome {
    TuiLaunchOutcome::Shutdown {
        signal,
        cleanup_error: cleanup.err().map(|error| format!("{error:#}")),
    }
}

/// How often a running account launch reconciles the profile with the live
/// `auth.json` when that account is also the current one.
const LIVE_AUTH_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Wait for a ChatGPT launch to exit, keeping the profile and the live
/// `auth.json` on the same token rotation meanwhile (see
/// [`profile::reconcile_live_with_profile`]).
async fn wait_for_account_child(
    child: &mut std::process::Child,
    shutdown: &mut ShutdownListener,
    alias: &str,
) -> std::io::Result<Result<std::process::ExitStatus, ShutdownSignal>> {
    let mut next_sync = tokio::time::Instant::now() + LIVE_AUTH_SYNC_INTERVAL;
    loop {
        tokio::select! {
            signal = shutdown.recv_signal() => return Ok(Err(signal)),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if let Some(status) = child.try_wait()? {
                    return Ok(Ok(status));
                }
                if tokio::time::Instant::now() >= next_sync {
                    sync_live_auth(alias).await;
                    next_sync = tokio::time::Instant::now() + LIVE_AUTH_SYNC_INTERVAL;
                }
            }
        }
    }
}

/// One best-effort reconcile pass; failures are logged and retried later.
async fn sync_live_auth(alias: &str) {
    let owned = alias.to_string();
    match tokio::task::spawn_blocking(move || profile::reconcile_live_with_profile(&owned)).await {
        Ok(Ok(Some(direction))) => {
            tracing::info!(alias, ?direction, "reconciled profile with live auth.json");
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => {
            tracing::debug!(alias, "live auth reconcile skipped: {error:#}");
        }
        Err(error) => tracing::warn!(alias, "live auth reconcile task failed: {error}"),
    }
}

async fn wait_for_child_or_shutdown(
    child: &mut std::process::Child,
    shutdown: &mut ShutdownListener,
) -> std::io::Result<Result<std::process::ExitStatus, ShutdownSignal>> {
    loop {
        tokio::select! {
            signal = shutdown.recv_signal() => return Ok(Err(signal)),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                if let Some(status) = child.try_wait()? {
                    return Ok(Ok(status));
                }
            }
        }
    }
}

/// Launch Codex for one alias from the TUI. Returns Codex's exit code instead of
/// terminating the codex-switch process on failure.
pub(crate) async fn launch_for_tui(
    alias: &str,
    model: Option<&str>,
    reasoning: ReasoningLaunch,
    extra_args: Vec<String>,
    shutdown: &mut ShutdownListener,
) -> Result<TuiLaunchOutcome> {
    launch_interactive(
        Some(alias),
        extra_args,
        false,
        false,
        model,
        reasoning,
        Some(shutdown),
    )
    .await
}

pub(crate) async fn launch_cmd(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    consume_card: bool,
    model: Option<&str>,
) -> Result<()> {
    finish_launch_cli(
        launch_interactive(
            alias,
            args,
            json,
            consume_card,
            model,
            ReasoningLaunch::Saved,
            None,
        )
        .await?,
    )
}

fn finish_launch_cli(outcome: TuiLaunchOutcome) -> Result<()> {
    let exit_code = match outcome {
        TuiLaunchOutcome::Exited(code) => code,
        TuiLaunchOutcome::Shutdown {
            signal,
            cleanup_error,
        } => {
            if let Some(error) = cleanup_error {
                eprintln!("Error while cleaning up interrupted launch: {error}");
            }
            signal.exit_code()
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn launch_interactive(
    alias: Option<&str>,
    args: Vec<String>,
    json: bool,
    consume_card: bool,
    model: Option<&str>,
    reasoning: ReasoningLaunch,
    tui_shutdown: Option<&mut ShutdownListener>,
) -> Result<TuiLaunchOutcome> {
    // Resolve and validate the exact executable once, before profile selection
    // can consume a reset card or provider launch can create a native run.
    let codex_command = ensure_codex_available()?;
    crate::codex_compat::ensure_launch_version(&codex_command)?;

    // A custom API provider profile takes a separate, simpler path: it has no
    // OAuth auth.json to stage, so it never touches ~/.codex/auth.json. It is
    // translated into `codex -c …` overrides with the key injected via the
    // environment. Auto-select (no alias) stays ChatGPT-only.
    if let Some(alias) = alias
        && provider::exists(alias)
    {
        let profile = provider::load(alias)?;
        return launch_provider(
            profile,
            codex_command,
            model,
            reasoning,
            args,
            json,
            tui_shutdown,
        )
        .await;
    }

    if alias.is_none() && provider_resume_requested(&args) {
        let provider_alias = select_provider_for_resume(json)?;
        let profile = provider::load(&provider_alias)?;
        return launch_provider(
            profile,
            codex_command,
            model,
            reasoning,
            args,
            json,
            tui_shutdown,
        )
        .await;
    }

    // Only the ChatGPT launch path uses file-backed OAuth credentials. Keep
    // this after both provider branches so provider keys remain independent.
    auth::ensure_file_credentials_store()?;

    let forwarded = chatgpt_codex_argv(model, reasoning, args);

    let mut revival_hint = None;
    let target_alias = match alias {
        Some(alias) => {
            let profiles = profile::list_profiles()?;
            if !profiles.iter().any(|profile| profile == alias) {
                anyhow::bail!("profile '{}' not found", alias);
            }
            alias.to_string()
        }
        None => {
            let card_policy = if consume_card {
                crate::commands::profile::CardPolicy::PreApproved
            } else if !json && std::io::stdin().is_terminal() {
                crate::commands::profile::CardPolicy::Prompt
            } else {
                crate::commands::profile::CardPolicy::Deny
            };
            let outcome = crate::commands::profile::select_best_profile(json, card_policy).await?;
            revival_hint = outcome.revival_hint;
            outcome.alias
        }
    };
    if let Some(hint) = &revival_hint
        && !json
    {
        user_println(&crate::commands::profile::revival_hint_message(hint));
    }

    let forwarded = if codex_argv_selects_server(&forwarded) {
        forwarded
    } else {
        embedded_codex_argv(codex_supports_no_daemon(&codex_command)?, forwarded)
    };

    // Each account runs in its own Codex home whose auth.json links to the
    // profile, so nothing shared is swapped and concurrent launches of
    // different accounts do not wait for each other.
    let account_home = {
        let alias = target_alias.clone();
        tokio::task::spawn_blocking(move || crate::account_home::prepare(&alias))
            .await
            .context("account home task panicked")?
            .with_context(|| format!("preparing the Codex home for profile '{target_alias}'"))?
    };
    sync_live_auth(&target_alias).await;

    let mut owned_shutdown;
    let interrupt = match tui_shutdown {
        Some(shutdown) => shutdown,
        None => {
            owned_shutdown = ShutdownListener::new()
                .context("registering shutdown handlers for the launched Codex")?;
            &mut owned_shutdown
        }
    };

    if !json {
        user_println(&format!("Launching Codex with profile '{target_alias}'..."));
    }

    let mut child = spawn_codex(
        &codex_command,
        &forwarded,
        account_home.sqlite_env(),
        json,
        Some(&account_home.codex_home),
    )
    .context("Failed to start codex")?;
    let pipes = take_codex_pipes(&mut child, json);

    let waited = wait_for_account_child(&mut child, interrupt, &target_alias).await;
    let status = match waited.context("waiting for codex")? {
        Ok(status) => status,
        Err(signal) => {
            terminate_child(&mut child, pipes);
            sync_live_auth(&target_alias).await;
            return Ok(shutdown_outcome(signal, Ok(())));
        }
    };
    // Fold in a refresh Codex made just before exiting.
    sync_live_auth(&target_alias).await;
    let captured = join_codex_pipes(pipes);

    let exit_code = child_exit_code(&status);

    if json {
        let mut payload = serde_json::json!({
            "ok": status.success(),
            "alias": target_alias,
            "action": "launched",
            "codex_home": account_home.codex_home,
            "exit_code": exit_code,
            "codex_stdout": captured.stdout,
            "codex_stderr": captured.stderr,
            "codex_stdout_truncated": captured.stdout_truncated,
            "codex_stderr_truncated": captured.stderr_truncated,
        });
        if let Some(model) = display_model(model, &forwarded) {
            payload["model"] = serde_json::Value::String(model);
        }
        if let Some(hint) = &revival_hint {
            payload["hint"] =
                serde_json::Value::String(crate::commands::profile::revival_hint_message(hint));
        }
        print_json(&payload);
    } else {
        user_println("codex exited");
    }

    Ok(TuiLaunchOutcome::Exited(exit_code))
}

/// Codex argv for a ChatGPT `launch`: optional `--model` and one-shot
/// reasoning are spliced after a Codex subcommand in `passthrough` (Codex
/// 0.149 ignores flags in front of `exec`). Interactive launch has no
/// subcommand, so those flags stay in front.
pub(crate) fn chatgpt_codex_argv(
    model: Option<&str>,
    reasoning: ReasoningLaunch,
    passthrough: Vec<String>,
) -> Vec<String> {
    let mut extra = Vec::new();
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        extra.push("--model".to_string());
        extra.push(model.to_string());
    }
    if let ReasoningLaunch::Effort(effort) = &reasoning {
        let effort = effort.trim();
        if !effort.is_empty() && !effort.eq_ignore_ascii_case("none") {
            extra.push("-c".to_string());
            extra.push(format!("model_reasoning_effort={effort}"));
        }
    }
    splice_after_subcommand(extra, passthrough)
}

/// Run a launched ChatGPT session in process.
///
/// Codex 0.157 and newer attaches an interactive session to an app-server
/// daemon of its `CODEX_HOME`, which keeps the account it loaded when it
/// started and outlives the session. `--no-daemon` keeps each launch a single
/// process that reads its account home's credentials and exits with the
/// session. It is a root option, so it goes before any subcommand.
/// An argv that already picks its server (`--no-daemon`, `--remote`, or the
/// daemon-only `agents` command) is left alone.
pub(crate) fn embedded_codex_argv(supports_no_daemon: bool, mut argv: Vec<String>) -> Vec<String> {
    if supports_no_daemon && !codex_argv_selects_server(&argv) {
        argv.insert(0, "--no-daemon".to_string());
    }
    argv
}

fn codex_argv_selects_server(argv: &[String]) -> bool {
    codex_syntax_indices(argv).into_iter().any(|index| {
        let arg = argv[index].as_str();
        arg == "--no-daemon" || arg == "--remote" || arg.starts_with("--remote=")
    }) || codex_subcommand_index(argv).is_some_and(|index| argv[index] == "agents")
}

/// `--no-daemon` exists since Codex 0.156; an older Codex rejects unknown
/// options, so its root help decides whether the flag can be passed.
fn codex_supports_no_daemon(command: &std::path::Path) -> Result<bool> {
    let mut probe = std::process::Command::new(command);
    probe
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Cold-starting a CLI wrapper can exceed two seconds on a busy machine.
    // This is a required routing decision, unlike the optional version probe.
    let output = crate::app_server::output_with_timeout(probe, std::time::Duration::from_secs(10))
        .context(
            "could not determine Codex account routing from --help; refusing to stage credentials",
        )?;
    no_daemon_support_from_help(output.status.success(), &output.stdout)
}

fn no_daemon_support_from_help(success: bool, stdout: &[u8]) -> Result<bool> {
    let help = String::from_utf8_lossy(stdout);
    if !success || !help.contains("Usage:") {
        anyhow::bail!(
            "Codex --help did not return usable help; refusing to stage credentials because account routing is unknown"
        );
    }
    Ok(help.contains("--no-daemon"))
}

/// Codex argv for a provider `launch`. Codex 0.149 applies `-c` on the
/// subcommand (`codex exec -c …`), not in front of it (`codex -c … exec` is
/// ignored and the child talks to api.openai.com). Interactive launch has no
/// subcommand, so overrides stay in front. If the caller already passed
/// `--model` / `-m`, drop our per-model `-c` pairs (`model`,
/// `model_reasoning_effort`, `web_search`) so they do not fight the one-shot
/// model; provider definition overrides stay.
pub(crate) fn provider_codex_argv(overrides: Vec<String>, passthrough: Vec<String>) -> Vec<String> {
    let mut overrides = overrides;
    if passthrough_sets_model(&passthrough) {
        strip_c_pair(&mut overrides, is_per_model_override);
    }
    splice_after_subcommand(overrides, passthrough)
}

fn splice_after_subcommand(overrides: Vec<String>, passthrough: Vec<String>) -> Vec<String> {
    let Some(idx) = codex_subcommand_index(&passthrough) else {
        let mut argv = overrides;
        argv.extend(passthrough);
        return argv;
    };
    // Codex 0.149 ignores options in front of the subcommand, so flags that
    // the user put before `exec` move after it along with our `-c` overrides.
    let mut argv = Vec::with_capacity(overrides.len() + passthrough.len());
    argv.push(passthrough[idx].clone());
    argv.extend(overrides);
    argv.extend(
        passthrough
            .into_iter()
            .enumerate()
            .filter_map(|(i, arg)| (i != idx).then_some(arg)),
    );
    argv
}

/// Exclude option values and everything after `--` before recognizing syntax.
/// A model, directory or prompt named `resume`/`agents` is not a subcommand.
fn codex_syntax_indices(args: &[String]) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut skip_next = false;
    let mut images = false;
    for (index, arg) in args.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            break;
        }
        if images && !arg.starts_with('-') {
            continue;
        }
        images = matches!(arg.as_str(), "-i" | "--image");
        indices.push(index);
        skip_next = !arg.contains('=')
            && (resume_option_takes_value(arg)
                || matches!(
                    arg.as_str(),
                    "--color" | "--output-schema" | "--output-last-message" | "-o"
                ));
    }
    indices
}

fn codex_subcommand_index(args: &[String]) -> Option<usize> {
    let index = codex_syntax_indices(args)
        .into_iter()
        .find(|&index| !args[index].starts_with('-'))?;
    crate::cli::is_codex_subcommand(&args[index]).then_some(index)
}

fn passthrough_sets_model(args: &[String]) -> bool {
    passthrough_model_value(args).is_some()
}

fn passthrough_model_value(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return None;
        }
        if arg == "--model" || arg == "-m" {
            return args.get(i + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix("--model=") {
            return Some(value.to_string());
        }
        i += 1;
    }
    None
}

fn display_model(cs_model: Option<&str>, passthrough: &[String]) -> Option<String> {
    passthrough_model_value(passthrough).or_else(|| {
        cs_model
            .filter(|model| !model.is_empty())
            .map(str::to_string)
    })
}

fn is_per_model_override(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(key, _)| matches!(key, "model" | "model_reasoning_effort" | "web_search"))
}

fn strip_c_pair(argv: &mut Vec<String>, value_matches: impl Fn(&str) -> bool) {
    let mut i = 0;
    while i + 1 < argv.len() {
        if argv[i] == "-c" && value_matches(&argv[i + 1]) {
            argv.drain(i..=i + 1);
            continue;
        }
        i += 1;
    }
}

fn spawn_codex(
    command: &std::path::Path,
    args: &[String],
    extra_env: Option<(String, String)>,
    json: bool,
    isolated_codex_home: Option<&std::path::Path>,
) -> std::io::Result<std::process::Child> {
    spawn_codex_with_capture(command, args, extra_env, json, isolated_codex_home, None)
}

fn spawn_codex_with_capture(
    command: &std::path::Path,
    args: &[String],
    extra_env: Option<(String, String)>,
    json: bool,
    codex_home: Option<&std::path::Path>,
    capture: Option<&ProviderOutput>,
) -> std::io::Result<std::process::Child> {
    let mut cmd = std::process::Command::new(command);
    cmd.args(args);
    if let Some(capture) = capture {
        cmd.stdin(std::process::Stdio::null())
            .stdout(capture.stdout.try_clone()?)
            .stderr(capture.stderr.try_clone()?);
    } else if json {
        // `--json launch` is non-interactive: inherited stdin is often a pipe
        // (not a TTY), and Codex exec then waits to append it as extra input.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
    }
    if let Some((name, value)) = extra_env {
        cmd.env(name, value);
    }
    if let Some(home) = codex_home {
        cmd.env("CODEX_HOME", home);
    }
    cmd.spawn()
}

/// Regular files keep the child writable after its launcher disappears.
struct ProviderOutput {
    stdout: std::fs::File,
    stderr: std::fs::File,
    stdout_path: std::path::PathBuf,
    stderr_path: std::path::PathBuf,
}

impl ProviderOutput {
    fn new(run_path: &std::path::Path) -> Result<Self> {
        let stdout_path = run_path.join("stdout.jsonl");
        let stderr_path = run_path.join("stderr.txt");
        auth::atomic_write_private(&stdout_path, b"")?;
        auth::atomic_write_private(&stderr_path, b"")?;
        Ok(Self {
            stdout: std::fs::OpenOptions::new().write(true).open(&stdout_path)?,
            stderr: std::fs::OpenOptions::new().write(true).open(&stderr_path)?,
            stdout_path,
            stderr_path,
        })
    }

    fn finish(self) -> Result<CapturedCodexIo> {
        fn read(path: &std::path::Path) -> Result<(String, bool)> {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take((CODEX_CAPTURE_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > CODEX_CAPTURE_LIMIT;
            bytes.truncate(CODEX_CAPTURE_LIMIT);
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            while text.len() > CODEX_CAPTURE_LIMIT {
                text.pop();
            }
            Ok((text, truncated))
        }
        drop(self.stdout);
        drop(self.stderr);
        let (stdout, stdout_truncated) = read(&self.stdout_path)?;
        let (stderr, stderr_truncated) = read(&self.stderr_path)?;
        // Successful completion no longer needs durable output; crashes retain it.
        std::fs::remove_file(&self.stdout_path)?;
        std::fs::remove_file(&self.stderr_path)?;
        Ok(CapturedCodexIo {
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }
}

struct CodexPipes {
    stdout: Option<std::thread::JoinHandle<CapturedBytes>>,
    stderr: Option<std::thread::JoinHandle<CapturedBytes>>,
}

fn terminate_child(child: &mut std::process::Child, pipes: CodexPipes) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = join_codex_pipes(pipes);
}

fn finish_provider_child_recording(
    child: &mut std::process::Child,
    recorded: Result<()>,
) -> Result<()> {
    if let Err(err) = recorded {
        // Keep the run lease and recoverable session files while stopping an
        // untracked child. Do not wait indefinitely if termination itself fails.
        if let Err(kill_error) = child.kill()
            && child.try_wait()?.is_none()
        {
            return Err(err).context(format!("recording provider run child pid; could not stop Codex: {kill_error}; retained session state"));
        }
        child
            .wait()
            .context("reaping Codex after its run metadata could not be saved")?;
        return Err(err)
            .context("recording provider run child pid; stopped Codex and retained session state");
    }
    Ok(())
}

struct CapturedCodexIo {
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

const CODEX_CAPTURE_LIMIT: usize = 1024 * 1024;

fn read_bounded(mut pipe: impl std::io::Read) -> CapturedBytes {
    let mut bytes = Vec::with_capacity(CODEX_CAPTURE_LIMIT.min(64 * 1024));
    let mut chunk = [0_u8; 16 * 1024];
    let mut truncated = false;
    while let Ok(read) = pipe.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = CODEX_CAPTURE_LIMIT.saturating_sub(bytes.len());
        let kept = remaining.min(read);
        bytes.extend_from_slice(&chunk[..kept]);
        truncated |= kept < read;
    }
    CapturedBytes { bytes, truncated }
}

fn take_codex_pipes(child: &mut std::process::Child, json: bool) -> CodexPipes {
    if !json {
        return CodexPipes {
            stdout: None,
            stderr: None,
        };
    }
    CodexPipes {
        stdout: child
            .stdout
            .take()
            .map(|mut pipe| std::thread::spawn(move || read_bounded(&mut pipe))),
        stderr: child
            .stderr
            .take()
            .map(|mut pipe| std::thread::spawn(move || read_bounded(&mut pipe))),
    }
}

fn join_codex_pipes(pipes: CodexPipes) -> CapturedCodexIo {
    fn into_string(handle: Option<std::thread::JoinHandle<CapturedBytes>>) -> (String, bool) {
        let captured = handle.and_then(|h| h.join().ok()).unwrap_or(CapturedBytes {
            bytes: Vec::new(),
            truncated: false,
        });
        let mut text = String::from_utf8_lossy(&captured.bytes).into_owned();
        let mut truncated = captured.truncated;
        if text.len() > CODEX_CAPTURE_LIMIT {
            let mut end = CODEX_CAPTURE_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        (text, truncated)
    }
    let (stdout, stdout_truncated) = into_string(pipes.stdout);
    let (stderr, stderr_truncated) = into_string(pipes.stderr);
    CapturedCodexIo {
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    }
}

/// Resolve the Codex command on PATH without running it: `codex --version`
/// writes PATH-alias helpers into `$CODEX_HOME/tmp`. The concrete path is
/// passed to the later spawn so preflight and execution use the same candidate.
fn ensure_codex_available() -> Result<std::path::PathBuf> {
    command_on_path("codex").ok_or_else(|| {
        anyhow::anyhow!("codex not found in PATH. Install: npm install -g @openai/codex")
    })
}

pub(crate) fn command_on_path(name: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let candidates = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&paths) {
        for file in &candidates {
            let candidate = dir.join(file);
            if candidate.is_file() {
                return if candidate.is_absolute() {
                    Some(candidate)
                } else {
                    std::env::current_dir().ok().map(|cwd| cwd.join(candidate))
                };
            }
        }
    }
    None
}

/// Codex's exit code, mapping a Unix signal death to `128 + signal`.
fn child_exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|s| 128 + s).unwrap_or(1)
        })
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

/// Launch Codex against a custom API provider profile.
///
/// Provider launches keep the user's resource home and select a private native
/// config profile. The parent never swaps default config or authentication.
async fn launch_provider(
    profile: ProviderProfile,
    codex_command: std::path::PathBuf,
    model: Option<&str>,
    reasoning: ReasoningLaunch,
    args: Vec<String>,
    json: bool,
    shutdown: Option<&mut ShutdownListener>,
) -> Result<TuiLaunchOutcome> {
    let mut owned_shutdown;
    let shutdown = match shutdown {
        Some(shutdown) => shutdown,
        None => {
            owned_shutdown = ShutdownListener::new()
                .context("registering shutdown handlers for provider launch")?;
            &mut owned_shutdown
        }
    };

    // Reclaim dead runs from crashed launches before touching this provider's.
    // A sweep failure is housekeeping trouble, never a reason to block launch.
    if let Err(error) = provider::sweep_dead_native_runs() {
        user_println(&format!("Warning: provider run cleanup failed: {error:#}"));
    }

    let resume = provider_resume_target(&args, &profile)?;
    let passthrough_model = passthrough_model_value(&args);
    // `--model` parsed by codex-switch selects a saved provider model. A model
    // forwarded after `--` belongs to Codex and intentionally keeps the
    // established one-shot behavior, including models outside the saved list.
    let resumed_config = match resume.as_ref() {
        Some((record, _)) if provider::ProviderLaunchProfile::is_native_run(&record.run_path)? => {
            provider::ProviderLaunchProfile::open_existing(&profile, &record.run_path)?
                .saved_config()?
        }
        _ => None,
    };
    let explicit_model = model.map(str::to_string);
    let resumed_model = resumed_config
        .as_ref()
        .and_then(|config| config.get("model"))
        .and_then(toml::Value::as_str);
    let reasoning = if model.is_none() && matches!(reasoning, ReasoningLaunch::Saved) {
        match resumed_config
            .as_ref()
            .and_then(|config| config.get("model_reasoning_effort"))
            .and_then(toml::Value::as_str)
        {
            Some("none") => ReasoningLaunch::Skip,
            Some(effort) => ReasoningLaunch::Effort(effort.to_string()),
            None => reasoning,
        }
    } else {
        reasoning
    };
    let selected_model = match (&resume, explicit_model.as_deref()) {
        (Some((session, _)), None) => {
            resumed_model.or(session.model.as_deref()).ok_or_else(|| {
                anyhow::anyhow!(
                    "provider session '{}' has no saved model; resume it with an explicit --model",
                    session.session_id
                )
            })?
        }
        (_, requested) => requested.unwrap_or(profile.default_model.as_str()),
    };
    let selected = profile.resolve_model(Some(selected_model))?.clone();
    let shown_model = passthrough_model.unwrap_or_else(|| selected.id.clone());
    // A saved denial is only a hint: re-check it live so an endpoint that has
    // since gained /responses is never refused on stale evidence. Supported or
    // unknown records cost nothing here.
    if profile.responses_support_for(&shown_model) == Some(false)
        && provider::recheck_cached_responses_denial(&profile, &shown_model).await?
    {
        anyhow::bail!(
            "Model '{}' on provider '{}' has no Codex Responses channel. A saved probe marked it unsupported and a fresh probe just confirmed it; probe again with `codex-switch provider probe {} --model {}` after the endpoint changes.",
            shown_model,
            profile.alias,
            profile.alias,
            shown_model,
        );
    }
    let (env_name, env_value) = profile.launch_env();
    if args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            arg == "--profile"
                || arg == "-p"
                || arg.starts_with("--profile=")
                || (arg.starts_with("-p") && arg.len() > 2)
        })
    {
        anyhow::bail!(
            "provider launch selects its own Codex profile; remove the forwarded --profile/-p option"
        );
    }
    let (mut legacy_session, mut native_session, _lease, codex_args, codex_home, run_path) =
        if let Some((record, resume_args)) = resume.as_ref()
            && !provider::ProviderLaunchProfile::is_native_run(&record.run_path)?
        {
            let lease = provider::ProviderRunLease::acquire(&record.run_path)?;
            let session = provider::ProviderCodexHome::open_existing(&profile, &record.run_path)?;
            let overrides = profile.codex_config_args_from_saved_catalog_at(
                Some(&selected.id),
                reasoning.clone(),
                &session.path,
            )?;
            let codex_home = session.path.clone();
            (
                Some(session),
                None,
                Some(lease),
                provider_codex_argv(overrides, resume_args.clone()),
                codex_home,
                record.run_path.clone(),
            )
        } else {
            let (session, lease, forwarded) = match resume {
                Some((record, resume_args)) => {
                    let lease = provider::ProviderRunLease::acquire(&record.run_path)?;
                    let session =
                        provider::ProviderLaunchProfile::open_existing(&profile, &record.run_path)?;
                    // Codex itself also refuses a second writer, but fail here
                    // with a clear message instead of letting it error later.
                    if provider::native_session_writer_active(
                        &session.codex_home,
                        &record.session_id,
                    ) {
                        anyhow::bail!(
                            "provider session '{}' is still owned by a live Codex; resume it after that Codex exits",
                            record.session_id
                        );
                    }
                    (session, Some(lease), resume_args)
                }
                None => (
                    provider::ProviderLaunchProfile::begin(&profile)?,
                    None,
                    args.clone(),
                ),
            };
            let runtime_profile = profile.for_runtime_provider_id(&session.runtime_provider_id);
            let mut overrides = runtime_profile.codex_config_args_from_saved_catalog_at(
                Some(&selected.id),
                reasoning.clone(),
                &session.path,
            )?;
            if passthrough_sets_model(&forwarded) {
                strip_c_pair(&mut overrides, is_per_model_override);
            }
            session.write_config(&profile, &overrides, &selected.id)?;
            strip_c_pair(&mut overrides, is_per_model_override);
            // Native --model wins over project defaults at startup; /model still
            // updates the active thread and saves its next-launch choice.
            if !passthrough_sets_model(&forwarded) {
                overrides.extend(["--model".to_string(), selected.id.clone()]);
            }
            overrides.extend(["--profile".to_string(), session.profile_name.clone()]);
            let codex_home = session.codex_home.clone();
            let run_path = session.path.clone();
            (
                None,
                Some(session),
                lease,
                provider_codex_argv(overrides, forwarded),
                codex_home,
                run_path,
            )
        };

    if !json {
        let reasoning_note = match &reasoning {
            ReasoningLaunch::Saved => String::new(),
            ReasoningLaunch::Skip => " reasoning=(skip)".to_string(),
            ReasoningLaunch::Effort(effort) => format!(" reasoning={effort}"),
        };
        user_println(&format!(
            "Launching Codex with provider '{}' ({}{})...",
            profile.alias, shown_model, reasoning_note
        ));
    }

    let capture = if json {
        Some(ProviderOutput::new(&run_path)?)
    } else {
        None
    };
    let mut child = match spawn_codex_with_capture(
        &codex_command,
        &codex_args,
        Some((env_name, env_value)),
        json,
        Some(&codex_home),
        capture.as_ref(),
    ) {
        Ok(child) => child,
        Err(err) => {
            if let Some(session) = legacy_session.as_mut() {
                session
                    .restore()
                    .context("merging legacy Codex config after spawn failure")?;
            }
            // Fresh native runs self-clean on drop; resumed runs keep state.
            return Err(err).context("Failed to start Codex");
        }
    };
    // The child owns its rollout now; record its pid so a crashed launcher's
    // next resume attempt can refuse to double a still-running Codex.
    if let Some(session) = native_session.as_mut() {
        session.disarm();
        let recorded = session.set_child_pid(&profile, &selected.id, child.id());
        finish_provider_child_recording(&mut child, recorded)?;
    }
    let pipes = take_codex_pipes(&mut child, json);

    let status = match wait_for_child_or_shutdown(&mut child, shutdown)
        .await
        .context("waiting for Codex")?
    {
        Ok(status) => status,
        Err(signal) => {
            terminate_child(&mut child, pipes);
            let cleanup = match legacy_session.as_mut() {
                Some(session) => session
                    .restore()
                    .context("merging legacy Codex config after provider shutdown"),
                None => Ok(()),
            };
            return Ok(shutdown_outcome(signal, cleanup));
        }
    };
    if let Some(session) = legacy_session.as_mut() {
        session
            .restore()
            .context("merging legacy Codex config after provider launch")?;
    }
    if let Some(session) = native_session.as_mut() {
        session
            .clear_child_pid(&profile, &selected.id)
            .context("clearing provider run child pid")?;
    }
    let captured = match capture {
        Some(capture) => capture.finish()?,
        None => join_codex_pipes(pipes),
    };
    let exit_code = child_exit_code(&status);

    if json {
        print_json(&serde_json::json!({
            "ok": status.success(),
            "alias": profile.alias,
            "action": "launched",
            "provider": profile.provider_id,
            "model": shown_model,
            "exit_code": exit_code,
            "codex_stdout": captured.stdout,
            "codex_stderr": captured.stderr,
            "codex_stdout_truncated": captured.stdout_truncated,
            "codex_stderr_truncated": captured.stderr_truncated,
        }));
    } else {
        user_println("codex exited");
    }

    Ok(TuiLaunchOutcome::Exited(exit_code))
}

fn provider_resume_requested(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "resume")
}

fn select_provider_for_resume(json: bool) -> Result<String> {
    let providers = provider::list_providers()?;
    if providers.is_empty() {
        anyhow::bail!("provider alias is required for resume; no providers are configured")
    }
    if !io::stdin().is_terminal() || json {
        anyhow::bail!(
            "provider alias is required for resume when stdin is not interactive; choose one of: {}",
            providers.join(", ")
        )
    }
    user_println("Select a provider alias for resume:");
    for (index, alias) in providers.iter().enumerate() {
        user_println(&format!("  {}. {alias}", index + 1));
    }
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("reading provider alias")?;
    let index = input
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|index| (1..=providers.len()).contains(index))
        .ok_or_else(|| anyhow::anyhow!("invalid provider selection"))?;
    Ok(providers[index - 1].clone())
}

fn provider_resume_target(
    args: &[String],
    profile: &ProviderProfile,
) -> Result<Option<(provider::ProviderSession, Vec<String>)>> {
    let Some(command_at) = codex_subcommand_index(args) else {
        return Ok(None);
    };
    let resume_at = if args[command_at] == "resume" {
        command_at
    } else if args[command_at] == "exec" {
        let Some(nested) = codex_subcommand_index(&args[command_at + 1..]) else {
            return Ok(None);
        };
        let nested = command_at + 1 + nested;
        if args[nested] != "resume" {
            return Ok(None);
        }
        nested
    } else {
        return Ok(None);
    };
    let root = auth::app_home()?.join("provider-runs");
    let index = provider::ProviderSessionIndex::rebuild(&root, &profile.identity_id)?;
    let all = args.iter().any(|arg| arg == "--all");
    let include_noninteractive = args.iter().any(|arg| arg == "--include-non-interactive");
    let filter = provider::ProviderResumeFilter {
        cwd: if all {
            None
        } else {
            std::env::current_dir().ok()
        },
        all,
        include_noninteractive,
    };
    let positional = resume_positional_indices(args, resume_at);
    let has_last = resume_flag_before_separator(args, resume_at, "--last");
    let selector = if has_last {
        None
    } else {
        positional
            .first()
            .and_then(|index| args.get(*index))
            .cloned()
    };
    let record = if has_last {
        index.last(&filter)?.clone()
    } else if let Some(selector) = selector {
        index
            .find_by_session_id(&selector)
            .or_else(|_| index.find_unique_name(&selector))?
            .clone()
    } else {
        select_provider_session(&index, &filter)?.clone()
    };
    let rewritten = rewrite_provider_resume_args(args, resume_at, &record.session_id);
    Ok(Some((record, rewritten)))
}

fn resume_option_takes_value(arg: &str) -> bool {
    [
        "-c",
        "--config",
        "-m",
        "--model",
        "-C",
        "--cd",
        "--sandbox",
        "-s",
        "--remote",
        "--remote-auth-token-env",
        "--local-provider",
        "-p",
        "--profile",
        "--add-dir",
        "-a",
        "--ask-for-approval",
        "--enable",
        "--disable",
    ]
    .iter()
    .any(|option| arg == *option || arg.starts_with(&format!("{option}=")))
}

fn resume_positional_indices(args: &[String], resume_at: usize) -> Vec<usize> {
    let mut positional = Vec::new();
    let mut after_separator = false;
    let mut skip_next = false;
    let mut consuming_images = false;
    for (index, item) in args.iter().enumerate().skip(resume_at + 1) {
        let arg = item.as_str();
        if skip_next {
            skip_next = false;
            continue;
        }
        if !after_separator && arg == "--" {
            after_separator = true;
            continue;
        }
        if !after_separator && consuming_images && !arg.starts_with('-') {
            continue;
        }
        consuming_images = false;
        if !after_separator && arg.starts_with('-') {
            if matches!(arg, "-i" | "--image") {
                consuming_images = true;
                continue;
            }
            if resume_option_takes_value(arg) && !arg.contains('=') {
                skip_next = true;
            }
            continue;
        }
        positional.push(index);
    }
    positional
}

fn resume_flag_before_separator(args: &[String], resume_at: usize, flag: &str) -> bool {
    args[resume_at + 1..]
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == flag)
}

fn select_provider_session<'a>(
    index: &'a provider::ProviderSessionIndex,
    filter: &provider::ProviderResumeFilter,
) -> Result<&'a provider::ProviderSession> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("provider resume requires a session id or name when stdin is not interactive")
    }
    let sessions = index.filtered(filter);
    if sessions.is_empty() {
        anyhow::bail!("no provider session matches the requested scope")
    }
    user_println("Select a provider session:");
    for (position, session) in sessions.iter().enumerate() {
        let label = if session.name.is_empty() {
            session.session_id.as_str()
        } else {
            session.name.as_str()
        };
        user_println(&format!(
            "  {}. {}  {}  {}",
            position + 1,
            label,
            session.session_id,
            session.updated_at
        ));
    }
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("reading provider session selection")?;
    resolve_provider_session_selection(index, &sessions, input.trim())
}

fn resolve_provider_session_selection<'a>(
    index: &'a provider::ProviderSessionIndex,
    displayed: &[&'a provider::ProviderSession],
    selector: &str,
) -> Result<&'a provider::ProviderSession> {
    if let Ok(position) = selector.parse::<usize>() {
        let index = displayed_selection_index(position, displayed.len())?;
        return displayed
            .get(index)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("invalid provider session selection"));
    }
    index
        .find_by_session_id(selector)
        .or_else(|_| index.find_unique_name(selector))
}

fn displayed_selection_index(position: usize, len: usize) -> Result<usize> {
    position
        .checked_sub(1)
        .filter(|index| *index < len)
        .ok_or_else(|| anyhow::anyhow!("invalid provider session selection"))
}

fn rewrite_provider_resume_args(
    args: &[String],
    resume_at: usize,
    session_id: &str,
) -> Vec<String> {
    let had_last = resume_flag_before_separator(args, resume_at, "--last");
    let last_at = had_last.then(|| {
        (resume_at + 1..args.len())
            .take_while(|index| args[*index] != "--")
            .find(|index| args[*index] == "--last")
            .expect("--last was found before the separator")
    });
    let mut rewritten = args
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != last_at)
        .map(|(_, arg)| arg.clone())
        .collect::<Vec<_>>();
    let resume_at = rewritten
        .iter()
        .position(|arg| arg == "resume")
        .unwrap_or(resume_at);
    let positional = resume_positional_indices(&rewritten, resume_at);
    if had_last {
        if let Some(index) = positional.first() {
            rewritten.insert(*index, session_id.to_string());
        } else {
            rewritten.push(session_id.to_string());
        }
    } else if let Some(index) = positional.first() {
        rewritten[*index] = session_id.to_string();
    } else {
        rewritten.push(session_id.to_string());
    }
    rewritten
}

#[cfg(test)]
mod tests {
    use super::{
        chatgpt_codex_argv, displayed_selection_index, embedded_codex_argv, ensure_codex_available,
        passthrough_model_value, provider_codex_argv, resume_flag_before_separator,
        resume_positional_indices, rewrite_provider_resume_args, shutdown_outcome,
    };
    // Only the permission assertions call this, and those are unix-only, so an
    // unconditional import is dead on Windows and fails `-D warnings` there.
    #[cfg(windows)]
    use super::spawn_codex;
    #[cfg(unix)]
    use super::{CodexPipes, terminate_child};

    #[test]
    fn unknown_daemon_support_cannot_silently_stage_an_account() {
        assert!(super::no_daemon_support_from_help(false, b"Usage: codex --no-daemon").is_err());
        assert!(super::no_daemon_support_from_help(true, b"").is_err());
        assert!(super::no_daemon_support_from_help(true, b"wrapper failed").is_err());
        assert!(
            !super::no_daemon_support_from_help(
                true,
                b"Usage: codex [OPTIONS]\nOptions:\n --model"
            )
            .unwrap()
        );
        assert!(
            super::no_daemon_support_from_help(
                true,
                b"Usage: codex [OPTIONS]\nOptions:\n --no-daemon"
            )
            .unwrap()
        );
    }

    #[test]
    fn failed_provider_pid_recording_stops_and_reaps_the_spawned_child() {
        #[cfg(windows)]
        let mut command = {
            let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            let mut command = std::process::Command::new(
                std::path::PathBuf::from(windows)
                    .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
            );
            command.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = std::process::Command::new("/bin/sleep");
            command.arg("30");
            command
        };
        let mut child = command.spawn().unwrap();
        let err = super::finish_provider_child_recording(
            &mut child,
            Err(anyhow::anyhow!("metadata write denied")),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("metadata write denied"));
        assert!(
            child.try_wait().unwrap().is_some(),
            "an unrecorded process must not survive launch failure"
        );
    }

    #[test]
    fn embedded_argv_runs_codex_without_the_shared_daemon() {
        let argv = ["exec", "--json", "review"].map(str::to_string).to_vec();
        assert_eq!(
            embedded_codex_argv(true, argv.clone()),
            ["--no-daemon", "exec", "--json", "review"].map(str::to_string)
        );
        assert_eq!(embedded_codex_argv(false, argv.clone()), argv);
        assert_eq!(
            embedded_codex_argv(true, Vec::new()),
            ["--no-daemon"].map(str::to_string)
        );
    }

    #[test]
    fn embedded_argv_respects_an_explicit_server_choice() {
        for argv in [
            vec!["--no-daemon", "exec", "review"],
            vec!["--remote", "ws://127.0.0.1:1"],
            vec!["--remote=ws://127.0.0.1:1"],
            vec!["agents"],
        ] {
            let argv: Vec<String> = argv.into_iter().map(str::to_string).collect();
            assert_eq!(embedded_codex_argv(true, argv.clone()), argv);
        }
        // A prompt that mentions agents is still a prompt.
        assert_eq!(
            embedded_codex_argv(true, vec!["list agents".to_string()]),
            ["--no-daemon", "list agents"].map(str::to_string)
        );
    }

    #[test]
    fn codex_syntax_does_not_treat_values_or_prompts_as_commands() {
        for raw in [
            vec!["--model", "resume"],
            vec!["-C", "exec", "review this"],
            vec!["review this", "exec"],
            vec!["--", "resume"],
        ] {
            let args = raw.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert_eq!(super::codex_subcommand_index(&args), None);
            let mut expected = vec!["-c".into(), "model=test".into()];
            expected.extend(args.clone());
            assert_eq!(
                provider_codex_argv(vec!["-c".into(), "model=test".into()], args.clone()),
                if args.first().is_some_and(|arg| arg == "--model") {
                    args
                } else {
                    expected
                }
            );
        }
        for raw in [
            vec!["exec", "agents"],
            vec!["--model", "agents"],
            vec!["--", "--remote"],
        ] {
            let args = raw.into_iter().map(str::to_string).collect::<Vec<_>>();
            let mut expected = vec!["--no-daemon".to_string()];
            expected.extend(args.clone());
            assert_eq!(embedded_codex_argv(true, args), expected);
        }
        let args = ["--model", "exec", "resume", "session"]
            .map(str::to_string)
            .to_vec();
        assert_eq!(super::codex_subcommand_index(&args), Some(2));
    }

    #[test]
    fn provider_session_picker_uses_one_based_bounded_positions() {
        assert_eq!(displayed_selection_index(1, 2).unwrap(), 0);
        assert_eq!(displayed_selection_index(2, 2).unwrap(), 1);
        assert!(displayed_selection_index(0, 2).is_err());
        assert!(displayed_selection_index(3, 2).is_err());
    }

    #[test]
    fn provider_resume_parser_skips_every_supported_option_value_and_image_list() {
        let args = [
            "resume",
            "--enable",
            "feature",
            "--profile",
            "team",
            "--add-dir",
            "extra",
            "--remote-auth-token-env",
            "TOKEN",
            "-i",
            "a.png",
            "b.png",
            "--sandbox",
            "workspace-write",
            "session-id",
            "continue",
        ]
        .map(str::to_string);
        assert_eq!(resume_positional_indices(&args, 0), vec![14, 15]);

        let separated = ["resume", "--", "--last", "prompt"].map(str::to_string);
        assert!(!resume_flag_before_separator(&separated, 0, "--last"));
        assert_eq!(resume_positional_indices(&separated, 0), vec![2, 3]);

        let prompt_flag = ["resume", "session", "--", "--last", "prompt"].map(str::to_string);
        assert_eq!(
            rewrite_provider_resume_args(&prompt_flag, 0, "exact-session"),
            ["resume", "exact-session", "--", "--last", "prompt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminating_a_launch_reaps_the_child_before_returning() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .expect("spawn child");

        terminate_child(
            &mut child,
            CodexPipes {
                stdout: None,
                stderr: None,
            },
        );

        assert!(
            child.try_wait().expect("query child").is_some(),
            "cleanup must not return while the launched process can still rewrite staged auth"
        );
    }

    #[test]
    fn cleanup_failure_does_not_consume_the_shutdown_signal() {
        let outcome = shutdown_outcome(
            crate::signals::ShutdownSignal::Terminate,
            Err(anyhow::anyhow!("restore failed")),
        );

        assert_eq!(
            outcome,
            super::TuiLaunchOutcome::Shutdown {
                signal: crate::signals::ShutdownSignal::Terminate,
                cleanup_error: Some("restore failed".to_string()),
            }
        );
    }

    #[test]
    fn chatgpt_argv_puts_model_after_a_codex_subcommand() {
        assert_eq!(
            chatgpt_codex_argv(
                Some("gpt-5.4"),
                crate::provider::ReasoningLaunch::Saved,
                vec!["exec".into(), "--json".into(), "hi".into()]
            ),
            ["exec", "--model", "gpt-5.4", "--json", "hi"]
        );
    }

    #[test]
    fn chatgpt_argv_puts_one_shot_reasoning_after_a_codex_subcommand() {
        assert_eq!(
            chatgpt_codex_argv(
                Some("gpt-5.4"),
                crate::provider::ReasoningLaunch::Effort("high".into()),
                vec!["exec".into(), "--json".into(), "hi".into()]
            ),
            [
                "exec",
                "--model",
                "gpt-5.4",
                "-c",
                "model_reasoning_effort=high",
                "--json",
                "hi"
            ]
        );
    }

    #[test]
    fn ensure_codex_available_looks_up_path_without_running_codex() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let fake = if cfg!(windows) {
            dir.path().join("codex.exe")
        } else {
            dir.path().join("codex")
        };
        std::fs::write(&fake, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", dir.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);
        ensure_codex_available().expect("a PATH file named codex is enough; do not run it");
    }

    #[test]
    fn ensure_codex_available_fails_when_codex_is_not_on_path() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", empty.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);
        let err = ensure_codex_available().unwrap_err().to_string();
        assert!(
            err.contains("codex not found in PATH"),
            "unexpected error: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn codex_cmd_found_by_preflight_is_used_for_spawn() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("codex.cmd"), "@echo off\r\nexit /b 0\r\n").unwrap();

        let previous = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", dir.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = Restore(previous);

        let command = ensure_codex_available().expect("codex.cmd must satisfy the PATH preflight");
        let mut child = spawn_codex(&command, &[], None, true, None)
            .expect("the command accepted by preflight must also be spawnable");
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn chatgpt_argv_without_model_is_passthrough_only() {
        assert_eq!(
            chatgpt_codex_argv(
                None,
                crate::provider::ReasoningLaunch::Skip,
                vec!["resume".into(), "--last".into()]
            ),
            ["resume", "--last"]
        );
    }

    #[test]
    fn passthrough_model_value_reads_long_short_and_equals_forms() {
        assert_eq!(
            passthrough_model_value(&["exec".into(), "--model".into(), "one-shot".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["-m".into(), "one-shot".into(), "exec".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["--model=one-shot".into(), "exec".into()]),
            Some("one-shot".into())
        );
        assert_eq!(
            passthrough_model_value(&["--".into(), "--model".into(), "not-a-flag".into()]),
            None
        );
    }

    #[test]
    fn provider_argv_puts_overrides_after_exec() {
        let argv = provider_codex_argv(
            vec![
                "-c".into(),
                r#"model="saved""#.into(),
                "-c".into(),
                r#"model_provider="p""#.into(),
            ],
            vec!["exec".into(), "--json".into(), "do".into()],
        );
        assert_eq!(
            argv,
            [
                "exec",
                "-c",
                r#"model="saved""#,
                "-c",
                r#"model_provider="p""#,
                "--json",
                "do"
            ]
        );
    }

    #[test]
    fn provider_argv_keeps_overrides_in_front_without_subcommand() {
        let argv = provider_codex_argv(
            vec!["-c".into(), r#"model_provider="p""#.into()],
            vec!["-s".into(), "read-only".into()],
        );
        assert_eq!(argv, ["-c", r#"model_provider="p""#, "-s", "read-only"]);
    }

    #[test]
    fn provider_argv_drops_per_model_overrides_when_passthrough_sets_model() {
        let argv = provider_codex_argv(
            vec![
                "-c".into(),
                r#"model_provider="p""#.into(),
                "-c".into(),
                r#"model="saved""#.into(),
                "-c".into(),
                "model_reasoning_effort=high".into(),
                "-c".into(),
                "web_search=disabled".into(),
                "-c".into(),
                r#"model_providers.p.base_url="https://example.test""#.into(),
            ],
            vec!["exec".into(), "--model".into(), "one-shot".into()],
        );
        assert_eq!(
            argv,
            [
                "exec",
                "-c",
                r#"model_provider="p""#,
                "-c",
                r#"model_providers.p.base_url="https://example.test""#,
                "--model",
                "one-shot",
            ]
        );
    }

    #[test]
    fn provider_argv_moves_flags_that_precede_exec_after_the_subcommand() {
        let argv = provider_codex_argv(
            vec![
                "-c".into(),
                r#"model_provider="p""#.into(),
                "-c".into(),
                r#"model="saved""#.into(),
            ],
            vec!["-m".into(), "one-shot".into(), "exec".into(), "hi".into()],
        );
        assert_eq!(
            argv,
            [
                "exec",
                "-c",
                r#"model_provider="p""#,
                "-m",
                "one-shot",
                "hi",
            ]
        );
    }

    #[test]
    fn provider_argv_keeps_c_model_when_double_dash_makes_model_a_prompt() {
        let argv = provider_codex_argv(
            vec!["-c".into(), r#"model="saved""#.into()],
            vec!["--".into(), "--model".into(), "not-a-flag".into()],
        );
        assert_eq!(
            argv,
            ["-c", r#"model="saved""#, "--", "--model", "not-a-flag"]
        );
    }

    /// A signal while Codex runs is reported to the caller, which terminates
    /// and reaps the child, instead of leaving Codex orphaned.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_termination_while_codex_runs_is_reported() {
        use super::wait_for_account_child;
        use crate::signals::{RAISE_LOCK, ShutdownListener, ShutdownSignal};
        use tokio::signal::unix::{SignalKind, signal};

        let _raise = RAISE_LOCK.lock().await;
        let mut shutdown = ShutdownListener::new().expect("shutdown listener");
        let mut witness = signal(SignalKind::terminate()).expect("witness listener");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();

        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        witness.recv().await;

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_account_child(&mut child, &mut shutdown, "work"),
        )
        .await
        .expect("the wait must observe the termination")
        .unwrap();
        assert_eq!(outcome, Err(ShutdownSignal::Terminate));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_exiting_codex_returns_its_status() {
        use super::wait_for_account_child;
        use crate::signals::{RAISE_LOCK, ShutdownListener};

        let _raise = RAISE_LOCK.lock().await;
        let mut shutdown = ShutdownListener::new().expect("shutdown listener");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let status = wait_for_account_child(&mut child, &mut shutdown, "work")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(3));
    }
}
