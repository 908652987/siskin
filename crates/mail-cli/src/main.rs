//! Developer CLI for the sync core. Not the product UI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::{DateTime, Local};
use mail_core::{presets, AccountSpec, Core, ReceiveProtocol, SecurityMode, SmtpSettings};
use mail_crypto::KeyringStore;
use mail_store::Store;

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
            "missing command: add-account | check | send | accounts | sync | watch | list | body | attachment | stats | export-key | import-key",
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
    let secrets = Arc::new(KeyringStore::new(SECRET_SERVICE));

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
            let to: Vec<String> = required(&args.flags, "to")?
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect();
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
            let outcome = core
                .send(&email, &to, &subject, &body)
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
            let mailbox = args
                .flags
                .get("mailbox")
                .cloned()
                .unwrap_or_else(|| "INBOX".to_string());
            let outcomes = core
                .sync(&email, &mailbox)
                .await
                .map_err(|e| e.to_string())?;
            for o in outcomes {
                println!(
                    "{} tier={:?} full={} applied={} inserted={} removed={} in {} ms",
                    o.mailbox, o.tier, o.full, o.applied, o.inserted, o.removed, o.duration_ms
                );
            }
        }
        "watch" => {
            let email = required(&args.flags, "email")?;
            let mailbox = args
                .flags
                .get("mailbox")
                .cloned()
                .unwrap_or_else(|| "INBOX".to_string());
            let max_events = args
                .flags
                .get("max-events")
                .and_then(|v| v.parse().ok())
                .unwrap_or(usize::MAX);
            core.watch(&email, &mailbox, max_events, |o| {
                println!(
                    "{} tier={:?} full={} applied={} inserted={} removed={} in {} ms",
                    o.mailbox, o.tier, o.full, o.applied, o.inserted, o.removed, o.duration_ms
                );
            })
            .await
            .map_err(|e| e.to_string())?;
        }
        "list" => {
            let email = required(&args.flags, "email")?;
            let store = core.store();
            let account = store
                .account_by_email(&email)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("unknown account {email}"))?;
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
                .list_messages(account.id, limit, offset)
                .map_err(|e| e.to_string())?
            {
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
                    if msg.flags.iter().any(|f| f == "\\Seen") {
                        " "
                    } else {
                        "*"
                    },
                    msg.from.first().cloned().unwrap_or_default(),
                    msg.subject.unwrap_or_default()
                );
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
