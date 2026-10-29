//! Real-server STARTTLS smoke tests, skipped unless explicitly enabled.
//!
//! Run with `MAIL_PROTO_STARTTLS_SMOKE=1 cargo test -p mail-proto --test starttls`.
//! Dummy credentials are used: success is proven by reaching the authentication
//! exchange, which can only happen after the TLS upgrade.

use mail_proto::pop3::{Pop3Config, Pop3Session};
use mail_proto::smtp::{SmtpConfig, SmtpSession};
use mail_proto::{ImapConfig, ImapSession, ProtoError, Security};

fn enabled() -> bool {
    std::env::var("MAIL_PROTO_STARTTLS_SMOKE").is_ok()
}

#[tokio::test]
async fn imap_starttls_reaches_authentication() {
    if !enabled() {
        return;
    }
    let result = ImapSession::connect(&ImapConfig {
        host: "imap.88.com".into(),
        port: 143,
        security: Security::StartTls,
        username: "probe@example.com".into(),
        password: "wrong-password".into(),
    })
    .await;
    match result {
        Err(ProtoError::Imap(_)) => {}
        Err(err) => panic!("IMAP STARTTLS failed before authentication: {err:?}"),
        Ok(_) => panic!("dummy credentials were accepted"),
    }
}

#[tokio::test]
async fn pop3_stls_reaches_authentication() {
    if !enabled() {
        return;
    }
    let result = Pop3Session::connect(&Pop3Config {
        host: "pop.88.com".into(),
        port: 110,
        security: Security::StartTls,
        username: "probe@example.com".into(),
        password: "wrong-password".into(),
    })
    .await;
    match result {
        Err(ProtoError::Unexpected(message)) if message.contains("authentication") => {}
        Err(err) => panic!("POP3 STLS failed before authentication: {err:?}"),
        Ok(_) => panic!("dummy credentials were accepted"),
    }
}

#[tokio::test]
async fn smtp_starttls_reaches_authentication() {
    if !enabled() {
        return;
    }
    let result = SmtpSession::connect(&SmtpConfig {
        host: "smtp.88.com".into(),
        port: 587,
        security: Security::StartTls,
        username: Some("probe@example.com".into()),
        password: Some("wrong-password".into()),
    })
    .await;
    match result {
        Err(ProtoError::Unexpected(message)) if message.contains("authentication") => {}
        Err(err) => panic!("SMTP STARTTLS failed before authentication: {err:?}"),
        Ok(_) => panic!("dummy credentials were accepted"),
    }
}
