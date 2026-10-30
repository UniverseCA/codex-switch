//! Per-account Codex homes for ChatGPT `launch`.
//!
//! Each saved profile gets `$CODEX_SWITCH_HOME/homes/<alias>/`, which becomes
//! the launched process's `CODEX_HOME`. Its `auth.json` is a symlink to the
//! profile's saved credentials, so Codex and codex-switch read and rotate one
//! file per account: Codex writes refreshed tokens through the link in place,
//! and reloads that same file before refreshing or after a 401, so a rotation
//! made by codex-switch is picked up instead of replaying a spent token.
//!
//! The shared Codex home stays the owner of user content. An allow-list of its
//! entries (configuration, instructions, skills, plugins, sessions and the locks
//! that guard them, …) is linked into every account home, and
//! `CODEX_SQLITE_HOME` points the SQLite databases at the shared home, so
//! sessions started under one account can be resumed under another. Everything
//! else — the app-server daemon socket, PATH helpers under `tmp/`, the model
//! cache, shell snapshots, logs — stays per account.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Shared-home files linked into every account home, present or not. A link to
/// a missing file is harmless: Codex treats it as absent, and creating the file
/// through the link (history, a first `config.toml` edit) lands in the shared
/// home where the next account will see it.
const SHARED_FILES: &[&str] = &[
    "config.toml",
    "AGENTS.md",
    "AGENTS.override.md",
    "hooks.json",
    "history.jsonl",
    // Session name index next to the shared rollouts.
    "session_index.jsonl",
    // rmcp-client's fallback store for MCP OAuth credentials.
    ".credentials.json",
];

/// Shared-home directories linked into every account home when they exist. A
/// dangling directory link would make Codex's own `create_dir_all` fail, so a
/// missing directory is only linked once the shared home has it.
const SHARED_DIRS: &[&str] = &[
    "sessions",
    "archived_sessions",
    "skills",
    "plugins",
    "prompts",
    "rules",
    "agents",
    "themes",
    "memories",
    "memories_v2",
    "memories_extensions",
    "worktrees",
    // Cross-process locks guarding the shared rollouts: per-thread writer
    // ownership, and migration journals.
    "thread-writer-locks",
    "rollout-migrations",
    // Plugin and marketplace caches with their sync lock, plus the rollout
    // compression and maintenance locks. (`tmp/`, without the dot, holds
    // per-process PATH helpers and stays per account.)
    ".tmp",
];

/// Shared directories created when missing. Codex creates these on first use,
/// so without this the first isolated session would make private copies that
/// later launches then have to leave alone: its rollouts, the bundled system
/// skills, and the locks that keep processes from writing one thread at once.
const CREATED_SHARED_DIRS: &[&str] = &[
    "sessions",
    "archived_sessions",
    "skills",
    ".tmp",
    "thread-writer-locks",
    "rollout-migrations",
];

/// Native Codex config profiles (`codex -p <name>`) are `<name>.config.toml`.
const NATIVE_PROFILE_SUFFIX: &str = ".config.toml";

/// Environment variable Codex reads for its state database location.
pub const SQLITE_HOME_ENV: &str = "CODEX_SQLITE_HOME";

#[derive(Debug, Clone)]
pub struct AccountHome {
    /// `CODEX_HOME` for the launched process.
    pub codex_home: PathBuf,
    /// The user's own Codex home that owns shared content.
    pub shared_home: PathBuf,
}

impl AccountHome {
    /// Environment the launched Codex needs beyond `CODEX_HOME`. An explicit
    /// `CODEX_SQLITE_HOME` in the caller's environment is inherited unchanged.
    pub fn sqlite_env(&self) -> Option<(String, String)> {
        if std::env::var_os(SQLITE_HOME_ENV).is_some_and(|value| !value.is_empty()) {
            return None;
        }
        Some((
            SQLITE_HOME_ENV.to_string(),
            self.shared_home.to_string_lossy().into_owned(),
        ))
    }
}

/// `$CODEX_SWITCH_HOME/homes/`
pub fn homes_dir() -> Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("homes"))
}

pub fn account_home_path(alias: &str) -> Result<PathBuf> {
    crate::profile::validate_alias(alias)?;
    Ok(homes_dir()?.join(alias))
}

/// Create or repair the account home for `alias` and return its paths.
///
/// Idempotent and safe to run while another session already uses the home:
/// correct links are left untouched, and only links that point elsewhere are
/// replaced. A real file or directory where a shared link belongs is treated
/// as a deliberate per-account override and kept.
pub fn prepare(alias: &str) -> Result<AccountHome> {
    let profile_auth = absolute(&crate::profile::profile_auth_path(alias)?)?;
    if !profile_auth.is_file() {
        return Err(crate::error::CsError::NotFound(alias.to_string()).into());
    }
    let shared_home = absolute(&crate::auth::user_codex_home()?)?;
    let codex_home = absolute(&account_home_path(alias)?)?;
    if codex_home.starts_with(&shared_home) || shared_home.starts_with(&codex_home) {
        anyhow::bail!(
            "account home {} and Codex home {} must not contain each other",
            codex_home.display(),
            shared_home.display()
        );
    }
    crate::profile::ensure_private_dir(&codex_home)?;

    link_auth(alias, &codex_home.join("auth.json"), &profile_auth)?;

    for name in CREATED_SHARED_DIRS {
        let dir = shared_home.join(name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating shared Codex directory {}", dir.display()))?;
    }
    for name in shared_entries(&shared_home)? {
        let target = shared_home.join(&name);
        let required = SHARED_FILES.contains(&name.as_str());
        if !required && !target.exists() {
            continue;
        }
        link_shared(&target, &codex_home.join(&name))?;
    }

    Ok(AccountHome {
        codex_home,
        shared_home,
    })
}

/// Every shared entry name: the built-in lists, native config profiles found
/// in the shared home, and `[launch] shared_paths`.
fn shared_entries(shared_home: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = SHARED_FILES
        .iter()
        .chain(SHARED_DIRS)
        .map(|name| name.to_string())
        .collect();
    if let Ok(entries) = std::fs::read_dir(shared_home) {
        let mut profiles: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(NATIVE_PROFILE_SUFFIX) && name != "config.toml")
            .collect();
        profiles.sort();
        names.extend(profiles);
    }
    for extra in &crate::config::get().launch.shared_paths {
        validate_shared_name(extra)?;
        names.push(extra.clone());
    }
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| seen.insert(name.clone()));
    Ok(names)
}

/// A configured shared entry must name one top-level entry of the Codex home,
/// and never the per-account credentials.
pub(crate) fn validate_shared_name(name: &str) -> Result<()> {
    let mut components = Path::new(name).components();
    let single_normal = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !single_normal || name.contains(['/', '\\']) {
        anyhow::bail!(
            "launch.shared_paths entry '{name}' must be a single file or directory name inside the Codex home"
        );
    }
    if name == "auth.json" {
        anyhow::bail!(
            "launch.shared_paths must not contain auth.json: credentials stay per account"
        );
    }
    Ok(())
}

/// Point `<home>/auth.json` at the profile's saved credentials.
///
/// `codex logout` inside an account home deletes the link, and a later
/// `codex login` there writes a regular file. Credentials for the profile's own
/// account are folded back into the profile when newer; anything else is
/// refused rather than silently discarded.
fn link_auth(alias: &str, link: &Path, profile_auth: &Path) -> Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {}
        Ok(meta) if meta.is_file() => {
            let local = crate::auth::read_auth(link)
                .with_context(|| format!("reading credentials left in {}", link.display()))?;
            crate::profile::absorb_account_home_auth(alias, &local).with_context(|| {
                format!(
                    "{} holds credentials that cannot be folded into profile '{alias}'; move it away or save it with `codex-switch import {}`",
                    link.display(),
                    link.display()
                )
            })?;
            std::fs::remove_file(link)
                .with_context(|| format!("removing absorbed credentials {}", link.display()))?;
        }
        Ok(_) => anyhow::bail!(
            "{} is not a file; remove it so the account home can link its credentials",
            link.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", link.display()));
        }
    }
    ensure_symlink(profile_auth, link, false)
}

fn link_shared(target: &Path, link: &Path) -> Result<()> {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {}
        Ok(_) => {
            tracing::warn!(
                path = %link.display(),
                shared = %target.display(),
                "keeping account-local entry instead of linking the shared one"
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", link.display()));
        }
    }
    ensure_symlink(target, link, target.is_dir())
}

/// Make `link` a symlink to `target`, replacing a link that points elsewhere.
/// A concurrent launch creating the same link is not an error.
fn ensure_symlink(target: &Path, link: &Path, is_dir: bool) -> Result<()> {
    if let Ok(existing) = std::fs::read_link(link) {
        if existing == target {
            return Ok(());
        }
        std::fs::remove_file(link)
            .with_context(|| format!("replacing stale link {}", link.display()))?;
    }
    match create_symlink(target, link, is_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::read_link(link) {
                Ok(existing) if existing == target => Ok(()),
                _ => Err(error).with_context(|| format!("linking {}", link.display())),
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("linking {} -> {}", link.display(), target.display())),
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path, _is_dir: bool) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path, is_dir: bool) -> std::io::Result<()> {
    // Requires Developer Mode or an elevated process on Windows.
    if is_dir {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    use super::{account_home_path, prepare, validate_shared_name};

    struct TestEnv {
        _lock: MutexGuard<'static, ()>,
        home: tempfile::TempDir,
        old: Vec<(&'static str, Option<OsString>)>,
    }

    impl TestEnv {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let vars = [
                "HOME",
                "CODEX_HOME",
                "CODEX_SWITCH_HOME",
                "CODEX_SQLITE_HOME",
            ];
            let old = vars
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect();
            unsafe {
                std::env::set_var("HOME", home.path());
                std::env::set_var("CODEX_HOME", home.path().join(".codex"));
                std::env::set_var("CODEX_SWITCH_HOME", home.path().join(".codex-switch"));
                std::env::remove_var("CODEX_SQLITE_HOME");
            }
            std::fs::create_dir_all(home.path().join(".codex")).unwrap();
            std::fs::write(
                home.path().join(".codex/config.toml"),
                "cli_auth_credentials_store = \"file\"\n",
            )
            .unwrap();
            Self {
                _lock: lock,
                home,
                old,
            }
        }

        fn codex(&self) -> std::path::PathBuf {
            self.home.path().join(".codex")
        }

        fn save_profile(&self, alias: &str, auth: &serde_json::Value) {
            let path = self
                .home
                .path()
                .join(".codex-switch/profiles")
                .join(alias)
                .join("auth.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, serde_json::to_string_pretty(auth).unwrap()).unwrap();
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                for (name, value) in &self.old {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    fn auth(account_id: &str, email: &str, refresh: &str, last_refresh: &str) -> serde_json::Value {
        use base64::Engine;
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
        });
        let encode = |value: &serde_json::Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        let id_token = format!(
            "{}.{}.sig",
            encode(&serde_json::json!({"alg": "none"})),
            encode(&claims)
        );
        serde_json::json!({
            "tokens": {
                "id_token": id_token,
                "access_token": "access",
                "refresh_token": refresh,
                "account_id": account_id,
            },
            "last_refresh": last_refresh,
        })
    }

    #[test]
    fn prepare_links_credentials_to_the_profile_and_shares_user_content() {
        let env = TestEnv::new();
        env.save_profile(
            "work",
            &auth("acct_w", "w@example.com", "r1", "2026-10-01T00:00:00Z"),
        );
        std::fs::write(env.codex().join("AGENTS.md"), "shared rules").unwrap();
        std::fs::create_dir_all(env.codex().join("skills/demo")).unwrap();
        std::fs::write(env.codex().join("work.config.toml"), "model = \"x\"\n").unwrap();

        let home = prepare("work").unwrap();
        let codex_home = &home.codex_home;
        assert_eq!(codex_home, &account_home_path("work").unwrap());

        let profile_auth = env
            .home
            .path()
            .join(".codex-switch/profiles/work/auth.json");
        assert_eq!(
            std::fs::read_link(codex_home.join("auth.json")).unwrap(),
            profile_auth
        );
        for name in [
            "config.toml",
            "AGENTS.md",
            "skills",
            "sessions",
            "work.config.toml",
        ] {
            assert_eq!(
                std::fs::read_link(codex_home.join(name)).unwrap(),
                env.codex().join(name),
                "{name} must link to the shared Codex home"
            );
        }
        // Sessions are created in the shared home so every account resumes them.
        assert!(env.codex().join("sessions").is_dir());
        // Absent optional directories are not linked (a dangling directory link
        // would break Codex's own create_dir_all).
        assert!(std::fs::symlink_metadata(codex_home.join("plugins")).is_err());
        // Per-account runtime state is never linked.
        assert!(std::fs::symlink_metadata(codex_home.join("models_cache.json")).is_err());
        assert_eq!(
            home.sqlite_env(),
            Some((
                "CODEX_SQLITE_HOME".to_string(),
                env.codex().to_string_lossy().into_owned()
            ))
        );
    }

    #[test]
    fn writes_through_the_account_home_reach_the_profile_and_the_shared_home() {
        let env = TestEnv::new();
        env.save_profile(
            "work",
            &auth("acct_w", "w@example.com", "r1", "2026-10-01T00:00:00Z"),
        );
        let home = prepare("work").unwrap();

        // Codex persists refreshed tokens by truncating auth.json in place.
        let refreshed = auth("acct_w", "w@example.com", "r2", "2026-10-02T00:00:00Z");
        std::fs::write(
            home.codex_home.join("auth.json"),
            serde_json::to_string(&refreshed).unwrap(),
        )
        .unwrap();
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                env.home
                    .path()
                    .join(".codex-switch/profiles/work/auth.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(saved["tokens"]["refresh_token"], "r2");

        // History created through the dangling link lands in the shared home.
        std::fs::write(home.codex_home.join("history.jsonl"), "{}\n").unwrap();
        assert!(env.codex().join("history.jsonl").is_file());

        // A codex-switch rotation (atomic replace of the profile) is what the
        // account home reads next.
        let rotated = auth("acct_w", "w@example.com", "r3", "2026-10-03T00:00:00Z");
        crate::auth::write_auth(
            &env.home
                .path()
                .join(".codex-switch/profiles/work/auth.json"),
            &rotated,
        )
        .unwrap();
        let seen = crate::auth::read_auth(&home.codex_home.join("auth.json")).unwrap();
        assert_eq!(seen["tokens"]["refresh_token"], "r3");
    }

    #[test]
    fn prepare_is_idempotent_and_repairs_stale_links_but_keeps_local_overrides() {
        let env = TestEnv::new();
        env.save_profile(
            "work",
            &auth("acct_w", "w@example.com", "r1", "2026-10-01T00:00:00Z"),
        );
        let home = prepare("work").unwrap();
        let codex_home = home.codex_home.clone();

        std::fs::remove_file(codex_home.join("config.toml")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/config.toml", codex_home.join("config.toml"))
            .unwrap();
        // A real file in place of the link is a deliberate per-account override.
        std::fs::remove_file(codex_home.join("AGENTS.md")).unwrap();
        std::fs::write(codex_home.join("AGENTS.md"), "account-local").unwrap();

        prepare("work").unwrap();
        prepare("work").unwrap();
        assert_eq!(
            std::fs::read_link(codex_home.join("config.toml")).unwrap(),
            env.codex().join("config.toml")
        );
        assert_eq!(
            std::fs::read_to_string(codex_home.join("AGENTS.md")).unwrap(),
            "account-local"
        );
    }

    #[test]
    fn a_fresh_login_left_in_the_account_home_is_absorbed_or_refused() {
        let env = TestEnv::new();
        env.save_profile(
            "work",
            &auth("acct_w", "w@example.com", "r1", "2026-10-01T00:00:00Z"),
        );
        let home = prepare("work").unwrap();
        let link = home.codex_home.join("auth.json");

        // `codex logout` + `codex login` as the same account: newer tokens win.
        std::fs::remove_file(&link).unwrap();
        let newer = auth("acct_w", "w@example.com", "r9", "2026-10-05T00:00:00Z");
        std::fs::write(&link, serde_json::to_string(&newer).unwrap()).unwrap();
        prepare("work").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            crate::auth::read_auth(&link).unwrap()["tokens"]["refresh_token"],
            "r9"
        );

        // A different account is refused and left in place.
        std::fs::remove_file(&link).unwrap();
        let other = auth("acct_o", "o@example.com", "x1", "2026-10-06T00:00:00Z");
        std::fs::write(&link, serde_json::to_string(&other).unwrap()).unwrap();
        let error = prepare("work").unwrap_err();
        assert!(
            format!("{error:#}").contains("codex-switch import"),
            "{error:#}"
        );
        assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
        let saved = crate::auth::read_auth(
            &env.home
                .path()
                .join(".codex-switch/profiles/work/auth.json"),
        )
        .unwrap();
        assert_eq!(saved["tokens"]["refresh_token"], "r9");
    }

    #[test]
    fn prepare_rejects_a_missing_profile() {
        let _env = TestEnv::new();
        assert!(prepare("ghost").is_err());
        assert!(!account_home_path("ghost").unwrap().exists());
    }

    #[test]
    fn explicit_sqlite_home_is_inherited() {
        let env = TestEnv::new();
        env.save_profile(
            "work",
            &auth("acct_w", "w@example.com", "r1", "2026-10-01T00:00:00Z"),
        );
        let home = prepare("work").unwrap();
        unsafe { std::env::set_var("CODEX_SQLITE_HOME", env.home.path().join("db")) };
        assert_eq!(home.sqlite_env(), None);
    }

    #[test]
    fn shared_names_must_be_single_non_credential_entries() {
        assert!(validate_shared_name("pets").is_ok());
        assert!(validate_shared_name(".mcp-cache").is_ok());
        for bad in ["", "..", "a/b", "/abs", "auth.json"] {
            assert!(validate_shared_name(bad).is_err(), "{bad}");
        }
    }
}
