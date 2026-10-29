//! POP3 body path: whole-message download -> MIME parse -> cache -> offline hit.

use std::sync::Arc;

use mail_core::{AccountSpec, Core, ReceiveProtocol, SecurityMode};
use mail_crypto::MemoryStore;
use mail_store::BodyState;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const MESSAGE: &str = concat!(
    "From: sender@example.com\r\n",
    "To: me@example.com\r\n",
    "Subject: body\r\n",
    "Message-ID: <body-1@example.com>\r\n",
    "MIME-Version: 1.0\r\n",
    "Content-Type: multipart/mixed; boundary=\"outer\"\r\n",
    "\r\n",
    "--outer\r\n",
    "Content-Type: multipart/alternative; boundary=\"alt\"\r\n",
    "\r\n",
    "--alt\r\n",
    "Content-Type: text/plain; charset=utf-8\r\n",
    "Content-Transfer-Encoding: quoted-printable\r\n",
    "\r\n",
    "=E4=BD=A0=E5=A5=BD world\r\n",
    "--alt\r\n",
    "Content-Type: text/html; charset=utf-8\r\n",
    "\r\n",
    "<p>Hi</p><script>alert(1)</script><img src=\"https://track.test/x.gif\">\r\n",
    "--alt--\r\n",
    "\r\n",
    "--outer\r\n",
    "Content-Type: application/pdf; name=\"report.pdf\"\r\n",
    "Content-Disposition: attachment; filename=\"report.pdf\"\r\n",
    "Content-Transfer-Encoding: base64\r\n",
    "\r\n",
    "aGVsbG8=\r\n",
    "--outer--\r\n",
);

async fn serve(mut socket: TcpStream) -> Vec<String> {
    let (read, mut write) = socket.split();
    let mut lines = BufReader::new(read).lines();
    let mut seen = Vec::new();
    write
        .write_all(b"+OK mock pop3\r\n")
        .await
        .expect("greeting");
    while let Some(line) = lines.next_line().await.expect("read") {
        seen.push(line.clone());
        let reply = if line == "CAPA" {
            "-ERR capabilities unsupported\r\n".to_string()
        } else if line.starts_with("USER ") || line.starts_with("PASS ") {
            "+OK\r\n".to_string()
        } else if line == "LIST" {
            format!("+OK\r\n1 {}\r\n.\r\n", MESSAGE.len())
        } else if line == "UIDL" {
            "+OK\r\n1 uid-body-1\r\n.\r\n".to_string()
        } else if line == "TOP 1 0" {
            let headers = MESSAGE.split("\r\n\r\n").next().unwrap_or_default();
            format!("+OK\r\n{headers}\r\n\r\n.\r\n")
        } else if line == "RETR 1" {
            format!("+OK\r\n{MESSAGE}.\r\n")
        } else if line == "QUIT" {
            "+OK bye\r\n".to_string()
        } else {
            format!("-ERR unknown {line}\r\n")
        };
        write.write_all(reply.as_bytes()).await.expect("reply");
        if line == "QUIT" {
            break;
        }
    }
    seen
}

async fn account(port: u16) -> Core {
    let core = Core::open_in_memory(Arc::new(MemoryStore::new())).expect("core");
    core.add_account(&AccountSpec {
        email: "me@example.com".into(),
        display_name: None,
        provider: "custom".into(),
        host: "127.0.0.1".into(),
        port,
        username: "me@example.com".into(),
        password: "secret".into(),
        protocol: ReceiveProtocol::Pop3,
        security: SecurityMode::Plaintext,
        smtp: None,
        sent_mailbox: None,
    })
    .expect("account");
    core
}

#[tokio::test]
async fn pop3_body_is_parsed_cached_and_served_offline() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    // Exactly two connections: one for sync, one for the body fetch. A third
    // attempt would block on accept and fail the test, proving the cache hit.
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.expect("accept");
            serve(socket).await;
        }
    });

    let core = account(port).await;
    let outcome = core.sync("me@example.com", "INBOX").await.expect("sync");
    assert_eq!(outcome[0].inserted, 1);

    let store = core.store();
    let account = store
        .account_by_email("me@example.com")
        .expect("lookup")
        .expect("account");
    let message = store
        .list_messages(account.id, 10, 0)
        .expect("list")
        .into_iter()
        .next()
        .expect("message");

    let content = core
        .fetch_body("me@example.com", message.id)
        .await
        .expect("body");
    assert_eq!(content.body_state, BodyState::Full);
    assert_eq!(content.text.as_deref(), Some("你好 world"));
    let html = content.html.as_deref().expect("html part");
    assert!(html.contains("<p>Hi</p>"));
    assert!(
        !html.contains("script"),
        "sanitizer must drop scripts: {html}"
    );
    assert!(
        !html.contains("track.test"),
        "remote images must not survive: {html}"
    );
    assert_eq!(content.attachments.len(), 1);
    assert_eq!(
        content.attachments[0].filename.as_deref(),
        Some("report.pdf")
    );
    assert!(content.attachments[0].downloaded);

    // The mock server has stopped accepting connections, so both calls below
    // can only succeed from the local cache.
    server.await.expect("server");
    let cached = core
        .fetch_body("me@example.com", message.id)
        .await
        .expect("cached body");
    assert_eq!(cached.text.as_deref(), Some("你好 world"));
    let attachment = core
        .fetch_attachment("me@example.com", message.id, &content.attachments[0].part)
        .await
        .expect("cached attachment");
    assert_eq!(attachment, b"hello");
}
