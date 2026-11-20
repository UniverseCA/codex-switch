# Native shared local resume

Enable `shared_local_resume = true` under `[launch]` in the codex-switch configuration for accounts that are allowed to share the entire local history directory. Each new interactive ChatGPT launch uses the selected account to continue the original thread. Use `codex-switch doctor` to check the effective CLI and feature status.

## Build a candidate

1. Check out the target official release tag in a separate Codex worktree. Record the tag and commit.
2. From that worktree, run `git apply --check /path/to/patches/codex-shared-local-resume.patch`, then apply the patch. Review the change against the tag if the patch needs to be refreshed.
3. Follow the upstream build requirements and the V8 verification steps in `.github/actions/setup-rusty-v8/action.yml` for the target tag. From `codex-rs`, build with `cargo build -p codex-cli --bin codex`. Some release tags leave the workspace crate versions at `0.0.0` in `Cargo.lock`. Review the resolved lockfile against the release and verify the third-party versions and sources before recording the build. Package `codex` with the matching official runtime resources, including the Linux bubblewrap helper. Check that `codex --version` matches the expected release and that `codex features list` includes `shared_local_resume_history`. Record the executable SHA-256.
4. Test in a separate Codex home and state database with shared test history. Start a ChatGPT launch, use **`/resume` in the conversation**, and verify the same thread ID opens with a second account. Check the default account filter, the shared-history setting, and normal `/model` behavior.
5. Install the verified candidate in `$CODEX_SWITCH_HOME/engines/<version>/bin/codex` or `bin/codex.exe` on Windows. Keep the matching official CLI on PATH. Turn on the setting and confirm the selected executable with `codex-switch doctor`.

The patch scopes shared discovery to local backends in the interactive resume picker. The original thread stays in its existing local store. If a future release has no compatible candidate, the launcher selects the official CLI on PATH. Use `codex-switch resume <alias>` for shared history where the app-server protocol supports it. Before switching a shared history store to a newer Codex release, keep a consistent backup of the session files and state database.
