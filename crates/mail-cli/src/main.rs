//! Developer CLI for the sync core. Not the product UI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::{DateTime, Local};
use mail_core::{
    presets, AccountSpec, Core, OutgoingAttachment, OutgoingMessage, ReceiveProtocol, SecurityMode,
    SmtpSettings, SyncOutcome, WatchOptions,
};
use mail_crypto::{FileStore, KeyringStore, SecretStore};
use mail_store::{Store, StoredMessage};

const SECRET_SERVICE: &str = "mail-client-dev";

struct Args {
    db: PathBuf,
    command: String,
    flags: HashMap<String, String>,
}

fn parse() -> Result<Args, String> {
    let mut db = PathBuf::from("mail.db");
    let mut command = None;
    let mut flags = HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        if arg == "--db" {
            db = PathBuf::from(it.next().ok_or("--db needs a value")?);
        } else if arg == "--password-stdin" {
            flags.insert("password-stdin".to_string(), "true".to_string());
        } else if let Some(name) = arg.strip_prefix("--") {
            let value = it.next().ok_or_else(|| format!("--{name} needs a value"))?;
            flags.insert(name.to_string(), value);
        } else if command.is_none() {
            command = Some(arg);
        } else {
            return Err(format!("unexpected argument: {arg}"));
        }
    }
    Ok(Args {
        db,
        command: command.ok_or(
            "missing command: add-account | check | send | accounts | sync | watch | mailboxes | list | search | trash | trash-purge | body | attachment | keywords | move | stats | retention | set-retention | changes | changes-compact | set-change-max-rows | devices | revoke-device | export-key | import-key",
        )?,
        flags,
    })
}

fn required(flags: &HashMap<String, String>, name: &str) -> Result<String, String> {
    flags
        .get(name)
        .cloned()
        .ok_or_else(|| format!("missing --{name}"))
}

fn comma_list(flags: &HashMap<String, String>, name: &str) -> Vec<String> {
    flags
        .get(name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The keychain is the default. macOS ties its grants to each binary's CDHash,
/// so ad-hoc signed dev builds prompt again after every rebuild; `--dev-secret-dir`
/// (or `EYRIE_DEV_SECRET_DIR`) switches to 0600 files for development only.
fn secret_store(flags: &HashMap<String, String>) -> Result<Arc<dyn SecretStore>, String> {
    let dir = flags
        .get("dev-secret-dir")
        .cloned()
        .or_else(|| std::env::var("EYRIE_DEV_SECRET_DIR").ok());
    match dir {
        Some(dir) => {
            eprintln!("using development secret store at {dir}; the keychain is not used");
            Ok(Arc::new(FileStore::new(dir).map_err(|e| e.to_string())?))
        }
        None => Ok(Arc::new(KeyringStore::new(SECRET_SERVICE))),
    }
}

/// Reads the account password from stdin with `--password-stdin`, so it never
/// reaches shell history or the process argument list.
fn password_from(flags: &HashMap<String, String>) -> Result<String, String> {
    if flag_bool(flags, "password-stdin", false) {
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("reading password from stdin: {e}"))?;
        let password = input.trim_end_matches(['\r', '\n']).to_string();
        if password.is_empty() {
            return Err("stdin carried an empty password".to_string());
        }
        return Ok(password);
    }
    required(flags, "password")
}

/// Message ids are local row ids, not IMAP UIDs.
fn message_id_from(flags: &HashMap<String, String>) -> Result<i64, String> {
    required(flags, "id")?
        .parse()
        .map_err(|_| "bad --id; expected a numeric local message id from `list`".to_string())
}

fn flag_bool(flags: &HashMap<String, String>, name: &str, default: bool) -> bool {
    flags
        .get(name)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

fn print_outcome(outcome: &SyncOutcome) {
    println!(
        "{} tier={:?} full={} applied={} inserted={} removed={} in {} ms",
        outcome.mailbox,
        outcome.tier,
        outcome.full,
        outcome.applied,
        outcome.inserted,
        outcome.removed,
        outcome.duration_ms
    );
}

fn account_id(store: &Store, email: &str) -> Result<i64, String> {
    store
        .account_by_email(email)
        .map_err(|e| e.to_string())?
        .map(|account| account.id)
        .ok_or_else(|| format!("unknown account {email}"))
}

fn print_message(msg: &StoredMessage) {
    let when = msg
        .sent_at
        .and_then(|ts| DateTime::from_timestamp(ts, 0))
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string());
    println!(
        "{when}  id={:<6} uid={:<6} [{}]  {} — {}",
        msg.id,
        msg.uid,
        if msg.flags.iter().any(|flag| flag == "\\Seen") {
            " "
        } else {
            "*"
        },
        msg.from.first().cloned().unwrap_or_default(),
        msg.subject.clone().unwrap_or_default()
    );
}

fn protocol_from(flags: &HashMap<String, String>) -> Result<ReceiveProtocol, String> {
    match flags
        .get("protocol")
        .map(String::as_str)
        .unwrap_or("imap")
        .to_ascii_lowercase()
        .as_str()
    {
        "imap" => Ok(ReceiveProtocol::Imap),
        "pop3" => Ok(ReceiveProtocol::Pop3),
        other => Err(format!("bad --protocol {other}; expected imap or pop3")),
    }
}

fn security_from(flags: &HashMap<String, String>, name: &str) -> Result<SecurityMode, String> {
    if let Some(value) = flags.get(name) {
        return match value.to_ascii_lowercase().as_str() {
            "tls" => Ok(SecurityMode::Tls),
            "starttls" => Ok(SecurityMode::StartTls),
            "plaintext" => Ok(SecurityMode::Plaintext),
            other => Err(format!(
                "bad --{name} {other}; expected tls, starttls or plaintext"
            )),
        };
    }
    if name == "security" && flags.get("tls").map(|v| v == "false").unwrap_or(false) {
        return Ok(SecurityMode::Plaintext);
    }
    Ok(SecurityMode::Tls)
}

fn account_spec(flags: &HashMap<String, String>) -> Result<AccountSpec, String> {
    let email = required(flags, "email")?;
    let provider = flags
        .get("provider")
        .cloned()
        .unwrap_or_else(|| "custom".to_string())
        .to_ascii_lowercase();
    let protocol = protocol_from(flags)?;
    let security = security_from(flags, "security")?;

    let preset = if provider == "custom" {
        None
    } else {
        Some(presets::hosts(&provider, &email).map_err(|e| e.to_string())?)
    };

    let host = match flags.get("host") {
        Some(host) => host.clone(),
        None => preset
            .as_ref()
            .map(|preset| match protocol {
                ReceiveProtocol::Imap => preset.imap.clone(),
                ReceiveProtocol::Pop3 => preset.pop3.clone(),
            })
            .ok_or_else(|| "missing --host".to_string())?,
    };
    let port = match flags.get("port") {
        Some(port) => port.parse().map_err(|_| "bad --port".to_string())?,
        None => presets::default_port(protocol, security),
    };

    let smtp_host = flags
        .get("smtp-host")
        .cloned()
        .or_else(|| preset.as_ref().map(|preset| preset.smtp.clone()));
    let smtp = match smtp_host {
        Some(host) => {
            let smtp_security = security_from(flags, "smtp-security")?;
            let port = match flags.get("smtp-port") {
                Some(port) => port.parse().map_err(|_| "bad --smtp-port".to_string())?,
                None => presets::default_smtp_port(smtp_security),
            };
            Some(SmtpSettings {
                host,
                port,
                security: Some(smtp_security),
                username: flags.get("smtp-username").cloned(),
            })
        }
        None => None,
    };

    if !flag_bool(flags, "insecure-plaintext", false)
        && (security == SecurityMode::Plaintext
            || smtp
                .as_ref()
                .map(|smtp| smtp.security() == SecurityMode::Plaintext)
                .unwrap_or(false))
    {
        return Err(
            "plaintext transport requires --insecure-plaintext true; use tls or starttls"
                .to_string(),
        );
    }

    Ok(AccountSpec {
        email: email.clone(),
        display_name: flags.get("name").cloned(),
        provider,
        host,
        port,
        username: flags.get("username").cloned().unwrap_or(email),
        password: password_from(flags)?,
        protocol,
        security,
        smtp,
        sent_mailbox: flags.get("sent-mailbox").cloned(),
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let args = parse()?;
    let secrets = secret_store(&args.flags)?;

    // Key commands must run before the core opens (and possibly creates) a key.
    match args.command.as_str() {
        "export-key" => {
            let encoded =
                mail_core::export_master_key(secrets.as_ref()).map_err(|e| e.to_string())?;
            eprintln!(
                "this line is your database master key: store it in your password manager, never in shell history or logs"
            );
            println!("{encoded}");
            return Ok(());
        }
        "import-key" => {
            let encoded = match args.flags.get("value") {
                Some(value) => value.clone(),
                None => std::io::read_to_string(std::io::stdin())
                    .map_err(|e| format!("reading key from stdin: {e}"))?,
            };
            let force = flag_bool(&args.flags, "force", false);
            let updated =
                mail_core::import_master_key(secrets.as_ref(), &encoded, Some(&args.db), force)
                    .map_err(|e| e.to_string())?;
            println!(
                "{}",
                if updated {
                    "master key imported"
                } else {
                    "master key already matches, nothing changed"
                }
            );
            return Ok(());
        }
        _ => {}
    }

    let core = Core::open(&args.db, secrets.clone()).map_err(|e| e.to_string())?;

    match args.command.as_str() {
        "add-account" => {
            let spec = account_spec(&args.flags)?;
            let id = core.add_account(&spec).map_err(|e| e.to_string())?;
            println!(
                "account id={id} email={} provider={} protocol={:?} security={:?}",
                spec.email, spec.provider, spec.protocol, spec.security
            );
        }
        "check" => {
            let email = required(&args.flags, "email")?;
            core.check(&email).await.map_err(|e| e.to_string())?;
            println!("login ok: {email}");
        }
        "send" => {
            let email = required(&args.flags, "email")?;
            let to = comma_list(&args.flags, "to");
            if to.is_empty() {
                return Err("--to needs at least one address".to_string());
            }
            let subject = args.flags.get("subject").cloned().unwrap_or_default();
            let body = match args.flags.get("body") {
                Some(body) => body.clone(),
                None => match args.flags.get("body-file") {
                    Some(path) => {
                        std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?
                    }
                    None => return Err("missing --body or --body-file".to_string()),
                },
            };
            let mut attachments = Vec::new();
            for path in comma_list(&args.flags, "attach") {
                let data = std::fs::read(&path).map_err(|e| format!("reading {path}: {e}"))?;
                let filename = PathBuf::from(&path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| path.clone());
                attachments.push(OutgoingAttachment {
                    filename,
                    content_type: None,
                    data,
                });
            }
            let message = OutgoingMessage {
                to,
                cc: comma_list(&args.flags, "cc"),
                bcc: comma_list(&args.flags, "bcc"),
                subject,
                body_text: body,
                attachments,
            };
            let outcome = core
                .send(&email, &message)
                .await
                .map_err(|e| e.to_string())?;
            println!("sent to {} recipient(s)", outcome.recipients);
            if let Some(mailbox) = outcome.saved_to {
                println!("saved to {mailbox}");
            }
            if let Some(err) = outcome.save_error {
                eprintln!("warning: sent but not saved to Sent: {err}");
            }
        }
        "accounts" => {
            let store: Arc<Store> = core.store();
            for account in store.list_accounts().map_err(|e| e.to_string())? {
                println!(
                    "{}#{} provider={} secret={}",
                    account.email, account.id, account.provider, account.secret_ref
                );
            }
        }
        "sync" => {
            let email = required(&args.flags, "email")?;
            match args.flags.get("mailbox") {
                Some(mailbox) => {
                    for outcome in core
                        .sync(&email, mailbox)
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        print_outcome(&outcome);
                    }
                }
                None => {
                    let report = core.sync_account(&email).await.map_err(|e| e.to_string())?;
                    for outcome in &report.outcomes {
                        print_outcome(outcome);
                    }
                    for failure in &report.failures {
                        eprintln!("warning: {} failed: {}", failure.mailbox, failure.error);
                    }
                    if !report.failures.is_empty() {
                        return Err(format!(
                            "{} mailbox(es) failed to sync",
                            report.failures.len()
                        ));
                    }
                }
            }
        }
        "watch" => {
            let email = required(&args.flags, "email")?;
            let max_events = args
                .flags
                .get("max-events")
                .and_then(|v| v.parse().ok())
                .unwrap_or(usize::MAX);
            match args.flags.get("mailbox") {
                Some(mailbox) => core
                    .watch(&email, mailbox, max_events, print_outcome)
                    .await
                    .map_err(|e| e.to_string())?,
                None => {
                    let defaults = WatchOptions::default();
                    let options = WatchOptions {
                        idle_sessions: args
                            .flags
                            .get("idle-sessions")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(defaults.idle_sessions),
                        sweep_seconds: args
                            .flags
                            .get("sweep-seconds")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(defaults.sweep_seconds),
                    };
                    core.watch_account(&email, max_events, options, print_outcome)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        "list" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = account_id(&store, &email)?;
            let limit = args
                .flags
                .get("limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let offset = args
                .flags
                .get("offset")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            for msg in store
                .list_messages(account, limit, offset)
                .map_err(|e| e.to_string())?
            {
                print_message(&msg);
            }
        }
        "search" => {
            let email = required(&args.flags, "email")?;
            let query = required(&args.flags, "query")?;
            let limit = args
                .flags
                .get("limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let store = core.store();
            let account = account_id(&store, &email)?;
            for msg in store
                .search_messages(account, &query, limit)
                .map_err(|e| e.to_string())?
            {
                print_message(&msg);
            }
        }
        "mailboxes" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = account_id(&store, &email)?;
            for mailbox in store.list_mailboxes(account).map_err(|e| e.to_string())? {
                let count = store
                    .count_messages_in_mailbox(mailbox.id)
                    .map_err(|e| e.to_string())?;
                println!(
                    "{} role={} state={} messages={count}",
                    mailbox.remote_name,
                    mailbox.role.as_deref().unwrap_or("-"),
                    if mailbox.missing_at.is_some() {
                        "missing"
                    } else {
                        "present"
                    }
                );
            }
        }
        "trash" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = account_id(&store, &email)?;
            let limit = args
                .flags
                .get("limit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let offset = args
                .flags
                .get("offset")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            for msg in store
                .list_trash(account, limit, offset)
                .map_err(|e| e.to_string())?
            {
                print_message(&msg);
            }
        }
        "trash-purge" => {
            let stats = core.purge_expired_trash().map_err(|e| e.to_string())?;
            println!(
                "purged mailboxes={} messages={}",
                stats.mailboxes, stats.messages
            );
        }
        "retention" => {
            let days = core.trash_retention_days().map_err(|e| e.to_string())?;
            println!("trash retention: {days} days");
        }
        "set-retention" => {
            let days: u32 = required(&args.flags, "days")?
                .parse()
                .map_err(|_| "--days expects a positive integer".to_string())?;
            core.set_trash_retention_days(days)
                .map_err(|e| e.to_string())?;
            println!("trash retention set to {days} days");
        }
        "set-change-max-rows" => {
            let rows: i64 = required(&args.flags, "rows")?
                .parse()
                .map_err(|_| "--rows expects a positive integer".to_string())?;
            let store = core.store();
            store
                .set_change_log_max_rows(rows)
                .map_err(|e| e.to_string())?;
            println!("change log compacts above {rows} rows");
        }
        "changes" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = account_id(&store, &email)?;
            let window = core.change_window(&email).map_err(|e| e.to_string())?;
            let count = store.count_changes(account).map_err(|e| e.to_string())?;
            let max_rows = store.change_log_max_rows().map_err(|e| e.to_string())?;
            let show = |seq: Option<i64>| seq.map_or_else(|| "-".to_string(), |s| s.to_string());
            println!(
                "latest_seq={} snapshot_seq={} oldest_change_seq={} changes={count} max_rows={max_rows}",
                window.latest_seq,
                show(window.snapshot_seq),
                show(window.oldest_seq)
            );
        }
        "changes-compact" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = account_id(&store, &email)?;
            let max_rows = match args.flags.get("max-rows") {
                Some(raw) => raw
                    .parse()
                    .map_err(|_| "--max-rows expects a positive integer".to_string())?,
                None => store.change_log_max_rows().map_err(|e| e.to_string())?,
            };
            match store
                .compact_changes(account, max_rows)
                .map_err(|e| e.to_string())?
            {
                Some(done) => println!(
                    "compacted into snapshot seq={} removed_changes={}",
                    done.snapshot_seq, done.removed_changes
                ),
                None => println!("change log has at most {max_rows} rows; nothing to compact"),
            }
        }
        "body" => {
            let email = required(&args.flags, "email")?;
            let id = message_id_from(&args.flags)?;
            let content = core
                .fetch_body(&email, id)
                .await
                .map_err(|e| e.to_string())?;
            eprintln!(
                "state={} text_bytes={} html_bytes={} attachments={}",
                content.body_state.as_str(),
                content.text.as_ref().map(String::len).unwrap_or(0),
                content.html.as_ref().map(String::len).unwrap_or(0),
                content.attachments.len()
            );
            for attachment in &content.attachments {
                eprintln!(
                    "attachment part={} type={} size={} downloaded={} name={}",
                    attachment.part,
                    attachment.content_type,
                    attachment.size,
                    attachment.downloaded,
                    attachment.filename.as_deref().unwrap_or("-")
                );
            }
            let body = if flag_bool(&args.flags, "html", false) {
                content.html.clone()
            } else {
                content.text.clone()
            };
            match body {
                Some(body) => println!("{body}"),
                None => return Err("message has no body part of the requested kind".to_string()),
            }
        }
        "attachment" => {
            let email = required(&args.flags, "email")?;
            let id = message_id_from(&args.flags)?;
            let part = required(&args.flags, "part")?;
            let data = core
                .fetch_attachment(&email, id, &part)
                .await
                .map_err(|e| e.to_string())?;
            match args.flags.get("out") {
                Some(path) => {
                    std::fs::write(path, &data).map_err(|e| format!("writing {path}: {e}"))?;
                    println!("wrote {} bytes to {path}", data.len());
                }
                None => {
                    use std::io::Write as _;
                    std::io::stdout()
                        .write_all(&data)
                        .map_err(|e| format!("writing attachment to stdout: {e}"))?;
                }
            }
        }
        "keywords" => {
            let email = required(&args.flags, "email")?;
            let id = message_id_from(&args.flags)?;
            let split = |name: &str| -> Vec<String> {
                args.flags
                    .get(name)
                    .map(|raw| {
                        raw.split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let add = split("add");
            let remove = split("remove");
            core.set_keywords(&email, id, &add, &remove)
                .await
                .map_err(|e| e.to_string())?;
            println!("keywords updated: add={add:?} remove={remove:?}");
        }
        "move" => {
            let email = required(&args.flags, "email")?;
            let id = message_id_from(&args.flags)?;
            let mailbox = required(&args.flags, "mailbox")?;
            let store = core.store();
            let account = account_id(&store, &email)?;
            let target = store
                .mailbox_by_name(account, &mailbox)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("unknown mailbox {mailbox}"))?;
            core.move_message(&email, id, target.id)
                .await
                .map_err(|e| e.to_string())?;
            println!("moved message {id} to {mailbox}");
        }
        "devices" => {
            let store = core.store();
            let devices = store.list_devices().map_err(|e| e.to_string())?;
            if devices.is_empty() {
                println!("no paired devices");
            }
            for device in devices {
                println!(
                    "id={} name={} created_at={} last_seen_at={}",
                    device.id,
                    device.name,
                    device.created_at,
                    device
                        .last_seen_at
                        .map_or_else(|| "-".to_string(), |at| at.to_string())
                );
            }
        }
        "revoke-device" => {
            let id: i64 = required(&args.flags, "id")?
                .parse()
                .map_err(|_| "--id expects a numeric device id from `devices`".to_string())?;
            let removed = core.store().revoke_device(id).map_err(|e| e.to_string())?;
            if removed {
                println!("device {id} revoked");
            } else {
                return Err(format!("device {id} not found"));
            }
        }
        "stats" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = store
                .account_by_email(&email)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("unknown account {email}"))?;
            let count = store
                .count_messages(account.id)
                .map_err(|e| e.to_string())?;
            let seq = store
                .latest_change_seq(account.id)
                .map_err(|e| e.to_string())?;
            let blob_bytes = store.blob_bytes().map_err(|e| e.to_string())?;
            println!(
                "account={} messages={} change_seq={} blob_bytes={}",
                account.email, count, seq, blob_bytes
            );
        }
        other => return Err(format!("unknown command: {other}")),
    }
    Ok(())
}
