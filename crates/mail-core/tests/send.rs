//! Send path: account settings -> composed message -> SMTP DATA.

use std::sync::Arc;

use mail_core::{AccountSpec, Core, ReceiveProtocol, SecurityMode, SendOutcome, SmtpSettings};
use mail_crypto::MemoryStore;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

async fn serve(mut socket: TcpStream) -> Vec<String> {
    let (read, mut write) = socket.split();
    let mut lines = BufReader::new(read).lines();
    let mut seen = Vec::new();
    write
        .write_all(b"220 mock smtp\r\n")
        .await
        .expect("greeting");
    let mut in_data = false;
    while let Some(line) = lines.next_line().await.expect("read") {
        seen.push(line.clone());
        if in_data {
            if line == "." {
                in_data = false;
                write.write_all(b"250 queued\r\n").await.expect("queued");
            }
            continue;
        }
        let reply = if line.starts_with("EHLO") {
            "250-mock\r\n250-AUTH PLAIN\r\n250 8BITMIME\r\n".to_string()
        } else if line.starts_with("AUTH PLAIN ") {
            "235 authenticated\r\n".to_string()
        } else if line.starts_with("MAIL FROM:") || line.starts_with("RCPT TO:") {
            "250 ok\r\n".to_string()
        } else if line == "DATA" {
            in_data = true;
            "354 go ahead\r\n".to_string()
        } else if line == "QUIT" {
            "221 bye\r\n".to_string()
        } else {
            "500 unknown\r\n".to_string()
        };
        write.write_all(reply.as_bytes()).await.expect("reply");
        if line == "QUIT" {
            break;
        }
    }
    seen
}

#[tokio::test]
async fn sending_composes_a_message_and_submits_it_over_smtp() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept");
        serve(socket).await
    });

    let core = Core::open_in_memory(Arc::new(MemoryStore::new())).expect("core");
    core.add_account(&AccountSpec {
        email: "me@example.com".into(),
        display_name: None,
        provider: "custom".into(),
        host: "127.0.0.1".into(),
        port: 1,
        username: "me@example.com".into(),
        password: "secret".into(),
        protocol: ReceiveProtocol::Pop3,
        security: SecurityMode::Plaintext,
        smtp: Some(SmtpSettings {
            host: "127.0.0.1".into(),
            port,
            security: Some(SecurityMode::Plaintext),
            username: None,
        }),
        sent_mailbox: None,
    })
    .expect("account");

    let outcome: SendOutcome = core
        .send(
            "me@example.com",
            &["peer@example.com".into()],
            "hello",
            "body",
        )
        .await
        .expect("send");
    assert_eq!(outcome.recipients, 1);
    assert!(outcome.saved_to.is_none(), "POP3 has no Sent mailbox");
    assert!(outcome.save_error.is_none());

    let seen = server.await.expect("server");
    assert!(seen.iter().any(|line| line == "MAIL FROM:<me@example.com>"));
    assert!(seen.iter().any(|line| line == "RCPT TO:<peer@example.com>"));
    assert!(seen.iter().any(|line| line == "Subject: hello"));
    assert!(seen.iter().any(|line| line == "To: peer@example.com"));
    assert!(seen.iter().any(|line| line == "Ym9keQ=="));
}
