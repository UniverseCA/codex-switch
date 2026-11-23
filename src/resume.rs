use crate::account_home::AccountHome;
use crate::{auth, jwt, profile, provider};
use anyhow::{Context, Result};
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;

const PAGE_SIZE: usize = 12;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    data: Vec<Session>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    id: String,
    name: Option<String>,
    preview: Option<String>,
    cwd: String,
    updated_at: i64,
    path: Option<PathBuf>,
}

pub(crate) struct Options {
    pub all: bool,
    pub last: bool,
    pub search: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Account {
    user_id: String,
    account_id: String,
}

impl Account {
    fn new(user_id: Option<&str>, account_id: Option<&str>) -> Option<Self> {
        Some(Self {
            user_id: user_id.filter(|id| !id.trim().is_empty())?.to_owned(),
            account_id: account_id.filter(|id| !id.trim().is_empty())?.to_owned(),
        })
    }

    fn for_profile(alias: &str) -> Option<Self> {
        let info = jwt::parse_account_info(
            &auth::read_auth(&profile::profile_auth_path(alias).ok()?).ok()?,
        );
        Self::new(info.user_id.as_deref(), info.account_id.as_deref())
    }
}

#[derive(Deserialize, Serialize)]
struct AccountRecord {
    account: Account,
    opened_at: i64,
}

pub(crate) struct Selection {
    pub id: String,
    pub cwd: PathBuf,
    pub alias: String,
    account: Option<Account>,
}

impl Selection {
    pub(crate) fn remember(self, opened_at: i64) -> Result<()> {
        let Some(account) = self.account else {
            return Ok(());
        };
        let path = account_record_path(&self.id)?;
        profile::ensure_private_dir(
            path.parent()
                .context("locating the session accounts directory")?,
        )?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        FileExt::lock(&lock)?;
        if !read_account_record(&path).is_some_and(|record| record.opened_at >= opened_at) {
            auth::atomic_write_private(
                &path,
                &serde_json::to_vec(&AccountRecord { account, opened_at })?,
            )?;
        }
        Ok(())
    }
}

fn account_record_path(id: &str) -> Result<PathBuf> {
    let key = hex::encode(Sha256::digest(id.as_bytes()));
    Ok(auth::app_home()?
        .join("session-accounts")
        .join(format!("{key}.json")))
}

fn read_account_record(path: &Path) -> Option<AccountRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

impl Session {
    fn account(&self) -> Option<Account> {
        if let Some(record) = account_record_path(&self.id)
            .ok()
            .as_deref()
            .and_then(read_account_record)
        {
            return Some(record.account);
        }
        self.creator_account()
    }

    fn creator_account(&self) -> Option<Account> {
        let meta = provider::read_rollout_metadata(self.path.as_deref()?).ok()?;
        let payload = meta.get("payload")?;
        if meta.get("type")?.as_str()? != "session_meta" || payload.get("id")?.as_str()? != self.id
        {
            return None;
        }
        Account::new(
            payload.get("creator_user_id").and_then(Value::as_str),
            payload.get("creator_account_id").and_then(Value::as_str),
        )
    }
}

struct SavedProfile<'a> {
    alias: &'a str,
    account: Option<Account>,
}

fn matching_profile<'a>(
    profiles: &'a [SavedProfile<'a>],
    account: Option<&Account>,
) -> Option<&'a str> {
    let account = account?;
    let mut matching = profiles
        .iter()
        .filter(|profile| profile.account.as_ref() == Some(account));
    let profile = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    Some(profile.alias)
}

pub(crate) async fn pick(
    codex: &Path,
    home: &AccountHome,
    aliases: &[String],
    explicit_alias: Option<&str>,
    options: Options,
) -> Result<Option<Selection>> {
    let Options { all, last, search } = options;
    let cwd = (!all).then(std::env::current_dir).transpose()?;
    let profiles: Vec<_> = aliases
        .iter()
        .map(|alias| SavedProfile {
            alias,
            account: Account::for_profile(alias),
        })
        .collect();
    let mut command = Command::new(codex);
    command
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", &home.codex_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some((name, value)) = home.sqlite_env() {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .context("starting the local Codex app-server")?;
    let mut input = child.stdin.take().context("opening the app-server input")?;
    let mut output = BufReader::new(
        child
            .stdout
            .take()
            .context("opening the app-server output")?,
    );
    request(
        &mut input,
        &mut output,
        0,
        "initialize",
        json!({"clientInfo": {"name": "codex-switch", "version": env!("CARGO_PKG_VERSION")}}),
    )
    .await?;
    send(&mut input, json!({"method": "initialized"})).await?;

    let mut cursor = None;
    let mut id = 1;
    loop {
        let page = list_page(
            &mut input,
            &mut output,
            id,
            cwd.as_deref(),
            search.as_deref(),
            cursor.as_deref(),
        )
        .await?;

        if page.data.is_empty() {
            println!("No matching local sessions.");
            return Ok(None);
        }
        if last {
            return select(&page.data[0], &profiles, explicit_alias);
        }
        let has_next = page.next_cursor.is_some();
        for (index, session) in page.data.iter().enumerate() {
            let title = session
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .or(session.preview.as_deref())
                .filter(|title| !title.trim().is_empty())
                .unwrap_or(&session.id);
            let updated = chrono::DateTime::from_timestamp(session.updated_at, 0)
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            let location = if all {
                format!("  [{}]", terminal_text(&session.cwd, 100))
            } else {
                String::new()
            };
            let account = session.account();
            let alias = matching_profile(&profiles, account.as_ref()).unwrap_or("unknown account");
            println!(
                "{:>2}. {updated}  {}  ({alias}){location}",
                index + 1,
                terminal_text(title, 100)
            );
        }
        loop {
            let navigation = if has_next { ", n for next page" } else { "" };
            print!("Session number{navigation}, or q to cancel: ");
            io::stdout().flush()?;
            let mut selection = String::new();
            if io::stdin().read_line(&mut selection)? == 0 || selection.trim() == "q" {
                return Ok(None);
            }
            let selection = selection.trim();
            if has_next && selection == "n" {
                break;
            }
            if let Ok(index) = selection.parse::<usize>()
                && let Some(session) = index.checked_sub(1).and_then(|i| page.data.get(i))
            {
                return select(session, &profiles, explicit_alias);
            }
            println!("Choose a number from 1 to {}.", page.data.len());
        }
        cursor = page.next_cursor;
        id += 1;
    }
}

fn select(
    session: &Session,
    profiles: &[SavedProfile<'_>],
    explicit_alias: Option<&str>,
) -> Result<Option<Selection>> {
    let account = session.account();
    let alias = match explicit_alias.or_else(|| matching_profile(profiles, account.as_ref())) {
        Some(alias) => alias.to_owned(),
        None => {
            println!(
                "No verified saved account for this session. Choose a ChatGPT profile to continue."
            );
            for (index, profile) in profiles.iter().enumerate() {
                println!("{:>2}. {}", index + 1, profile.alias);
            }
            loop {
                print!("Profile number, or q to cancel: ");
                io::stdout().flush()?;
                let mut selection = String::new();
                if io::stdin().read_line(&mut selection)? == 0 || selection.trim() == "q" {
                    return Ok(None);
                }
                if let Ok(index) = selection.trim().parse::<usize>()
                    && let Some(profile) = index.checked_sub(1).and_then(|i| profiles.get(i))
                {
                    break profile.alias.to_owned();
                }
                println!("Choose a number from 1 to {}.", profiles.len());
            }
        }
    };
    Ok(Some(Selection {
        id: session.id.clone(),
        cwd: PathBuf::from(&session.cwd),
        account: Account::for_profile(&alias),
        alias,
    }))
}

async fn list_page(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncBufRead + Unpin),
    id: u64,
    cwd: Option<&Path>,
    search: Option<&str>,
    cursor: Option<&str>,
) -> Result<Page> {
    serde_json::from_value(
        request(
            input,
            output,
            id,
            "thread/list",
            json!({
                "currentAccountOnly": false,
                "archived": false,
                "sortKey": "updated_at",
                "limit": PAGE_SIZE,
                "cwd": cwd,
                "searchTerm": search,
                "cursor": cursor,
            }),
        )
        .await?,
    )
    .context("decoding the local session list")
}

fn terminal_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

async fn send(input: &mut (impl AsyncWrite + Unpin), message: Value) -> Result<()> {
    let mut line = serde_json::to_vec(&message)?;
    line.push(b'\n');
    input.write_all(&line).await?;
    Ok(())
}

async fn request(
    input: &mut (impl AsyncWrite + Unpin),
    output: &mut (impl AsyncBufRead + Unpin),
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        send(input, json!({"id": id, "method": method, "params": params})).await?;
        let mut line = String::new();
        loop {
            line.clear();
            anyhow::ensure!(
                output.read_line(&mut line).await? != 0,
                "Codex app-server closed its connection"
            );
            let mut message: Value = serde_json::from_str(&line)?;
            if message["id"].as_u64() != Some(id) || message.get("method").is_some() {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error["message"].as_str().unwrap_or("unknown error");
                anyhow::bail!("{}", terminal_text(detail, 200));
            }
            return message
                .get_mut("result")
                .map(Value::take)
                .context("missing result in app-server response");
        }
    })
    .await
    .with_context(|| format!("Codex app-server timed out during {method}"))?
    .with_context(|| format!("requesting {method} from the local Codex app-server"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_creator_matches_the_same_user_and_workspace() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"session_meta","payload":{"id":"thread-a","creator_user_id":"user-a","creator_account_id":"team"}}"#,
        )
        .unwrap();
        let mut session = Session {
            id: "thread-a".to_owned(),
            name: None,
            preview: None,
            cwd: "/repo".to_owned(),
            updated_at: 0,
            path: Some(path),
        };
        let account = session.creator_account().unwrap();
        let profiles = [
            SavedProfile {
                alias: "different-user",
                account: Account::new(Some("user-b"), Some("team")),
            },
            SavedProfile {
                alias: "different-workspace",
                account: Account::new(Some("user-a"), Some("personal")),
            },
            SavedProfile {
                alias: "renamed-profile",
                account: Account::new(Some("user-a"), Some("team")),
            },
        ];
        assert_eq!(
            matching_profile(&profiles, Some(&account)),
            Some("renamed-profile")
        );
        assert_eq!(matching_profile(&profiles[..2], Some(&account)), None);
        session.id = "thread-b".to_owned();
        assert!(session.creator_account().is_none());
    }

    #[tokio::test]
    async fn list_page_explicitly_requests_shared_history() {
        let mut written = Vec::new();
        let mut replies = br#"{"method":"account/updated","params":{}}
{"id":2,"result":{"data":[{"id":"session-id","name":"Shared session","cwd":"/repo","updatedAt":12}],"nextCursor":"page-3"}}
"#
        .as_slice();
        let page = list_page(
            &mut written,
            &mut replies,
            2,
            None,
            Some("Shared"),
            Some("page-2"),
        )
        .await
        .unwrap();
        let sent: Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(sent["method"], "thread/list");
        assert_eq!(sent["params"]["currentAccountOnly"], false);
        assert_eq!(sent["params"]["archived"], false);
        assert_eq!(sent["params"]["cwd"], Value::Null);
        assert_eq!(sent["params"]["searchTerm"], "Shared");
        assert_eq!(sent["params"]["cursor"], "page-2");
        assert_eq!(page.data[0].id, "session-id");
        assert_eq!(page.next_cursor.as_deref(), Some("page-3"));
    }
}
