use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_imap::types::Capability;
use async_imap::Client;
use futures::TryStreamExt;
use serde_json::json;
use tokio::net::TcpStream;

struct Args {
    host: String,
    port: u16,
    user: String,
    pass: String,
    command: String,
    timeout: u64,
}

fn parse_args() -> Result<Args> {
    let mut host = "127.0.0.1".to_string();
    let mut port = 31143u16;
    let mut user = "vmail".to_string();
    let mut pass = "testpass".to_string();
    let mut command = "caps".to_string();
    let mut timeout = 30u64;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--host" => host = it.next().context("--host")?,
            "--port" => port = it.next().context("--port")?.parse()?,
            "--user" => user = it.next().context("--user")?,
            "--pass" => pass = it.next().context("--pass")?,
            "--timeout" => timeout = it.next().context("--timeout")?.parse()?,
            other => command = other.to_string(),
        }
    }
    Ok(Args {
        host,
        port,
        user,
        pass,
        command,
        timeout,
    })
}

fn cap_name(c: &Capability) -> String {
    match c {
        Capability::Imap4rev1 => "IMAP4rev1".to_string(),
        Capability::Auth(v) => format!("AUTH={v}"),
        Capability::Atom(v) => v.clone(),
    }
}

async fn login(args: &Args) -> Result<async_imap::Session<TcpStream>> {
    let tcp = TcpStream::connect((args.host.as_str(), args.port))
        .await
        .with_context(|| format!("tcp {}:{}", args.host, args.port))?;
    let client = Client::new(tcp);
    client
        .login(&args.user, &args.pass)
        .await
        .map_err(|(err, _client)| anyhow::anyhow!("login: {err}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let mut session = login(&args).await?;

    match args.command.as_str() {
        "caps" => {
            let caps = session.capabilities().await?;
            let standard = session.select("INBOX").await?;
            let condstore = session.select_condstore("INBOX").await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "capabilities": caps.iter().map(cap_name).collect::<Vec<_>>(),
                    "has_idle": caps.has_str("IDLE"),
                    "has_condstore": caps.has_str("CONDSTORE"),
                    "has_qresync": caps.has_str("QRESYNC"),
                    "has_move": caps.has_str("MOVE"),
                    "has_uidplus": caps.has_str("UIDPLUS"),
                    "exists": standard.exists,
                    "uidvalidity": standard.uid_validity,
                    "uidnext": standard.uid_next,
                    "highest_modseq": condstore.highest_modseq,
                }))?
            );
        }
        "fetch" => {
            let started = Instant::now();
            let select = session.select_condstore("INBOX").await?;
            let select_ms = started.elapsed().as_millis();

            let fetch_started = Instant::now();
            let mut stream = session
                .uid_fetch("1:*", "(UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)")
                .await?;
            let mut count = 0usize;
            while let Some(_item) = stream.try_next().await? {
                count += 1;
            }
            drop(stream);
            let fetch_ms = fetch_started.elapsed().as_millis();

            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "exists": select.exists,
                    "uidvalidity": select.uid_validity,
                    "uidnext": select.uid_next,
                    "highest_modseq": select.highest_modseq,
                    "fetched": count,
                    "select_ms": select_ms,
                    "fetch_ms": fetch_ms,
                    "messages_per_sec": if fetch_ms == 0 { 0.0 } else { count as f64 * 1000.0 / fetch_ms as f64 },
                }))?
            );
        }
        "condstore" => {
            let before = session.select_condstore("INBOX").await?;
            let modseq = before.highest_modseq.unwrap_or(1).saturating_sub(1);
            let started = Instant::now();
            let mut stream = session
                .uid_fetch("1:*", format!("(FLAGS) (CHANGEDSINCE {modseq})"))
                .await?;
            let mut changed = 0usize;
            while let Some(_item) = stream.try_next().await? {
                changed += 1;
            }
            drop(stream);
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "highest_modseq": before.highest_modseq,
                    "changed_since_baseline": changed,
                    "elapsed_ms": started.elapsed().as_millis(),
                }))?
            );
        }
        "idle" => {
            session.select("INBOX").await?;
            let mut handle = session.idle();
            handle.init().await?;

            let host = args.host.clone();
            let port = args.port;
            let user = args.user.clone();
            let pass = args.pass.clone();
            let appender = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(800)).await;
                let args = Args {
                    host,
                    port,
                    user,
                    pass,
                    command: "append".into(),
                    timeout: 0,
                };
                let mut s = login(&args).await?;
                let body = format!(
                    "From: probe@mail.test\r\nTo: vmail@example.com\r\nSubject: idle probe {}\r\nMessage-ID: <probe-{}@mail.test>\r\n\r\nhello\r\n",
                    chrono::Utc::now().timestamp_millis(),
                    chrono::Utc::now().timestamp_millis()
                );
                s.append("INBOX", Some("(\\Seen)"), None, body.as_bytes())
                    .await?;
                s.logout().await.ok();
                Ok::<(), anyhow::Error>(())
            });

            let started = Instant::now();
            let (wait, _interrupt) = handle.wait_with_timeout(Duration::from_secs(args.timeout));
            let response = wait.await?;
            let latency = started.elapsed().as_millis();
            let mut session = handle.done().await?;

            appender.await??;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "response": format!("{response:?}"),
                    "latency_ms": latency,
                }))?
            );
            session.logout().await.ok();
            return Ok(());
        }
        "append" => {
            let body = format!(
                "From: probe@mail.test\r\nTo: vmail@example.com\r\nSubject: idle probe {}\r\nMessage-ID: <probe-{}@mail.test>\r\n\r\nhello\r\n",
                chrono::Utc::now().timestamp_millis(),
                chrono::Utc::now().timestamp_millis()
            );
            let started = Instant::now();
            session
                .append("INBOX", Some("(\\Seen)"), None, body.as_bytes())
                .await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "appended_bytes": body.len(),
                    "elapsed_ms": started.elapsed().as_millis(),
                }))?
            );
        }
        other => anyhow::bail!("unknown command {other}"),
    }

    session.logout().await.ok();
    Ok(())
}
