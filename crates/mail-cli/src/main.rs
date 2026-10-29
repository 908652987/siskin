//! Developer CLI for the sync core. Not the product UI.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::{DateTime, Local};
use mail_core::{AccountSpec, Core};
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
            "missing command: add-account | accounts | sync | watch | list | stats | export-key | import-key",
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

fn flag_bool(flags: &HashMap<String, String>, name: &str, default: bool) -> bool {
    flags
        .get(name)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
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
            let spec = AccountSpec {
                email: required(&args.flags, "email")?,
                display_name: args.flags.get("name").cloned(),
                host: required(&args.flags, "host")?,
                port: required(&args.flags, "port")?
                    .parse()
                    .map_err(|_| "bad --port")?,
                tls: flag_bool(&args.flags, "tls", true),
                username: args
                    .flags
                    .get("username")
                    .cloned()
                    .unwrap_or_else(|| args.flags.get("email").cloned().unwrap_or_default()),
                password: required(&args.flags, "password")?,
            };
            let id = core.add_account(&spec).map_err(|e| e.to_string())?;
            println!("account id={id} email={} stored in keychain", spec.email);
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
                    "{when}  uid={:<6} [{}]  {} — {}",
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
            println!(
                "account={} messages={} change_seq={}",
                account.email, count, seq
            );
        }
        other => return Err(format!("unknown command: {other}")),
    }
    Ok(())
}
