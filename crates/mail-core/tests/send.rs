//! Send path: account settings -> composed message -> SMTP DATA.

use std::sync::Arc;

use mail_core::compose::{OutgoingAttachment, OutgoingMessage};
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

fn data_lines(seen: &[String]) -> Vec<String> {
    let start = seen
        .iter()
        .position(|line| line == "DATA")
        .expect("DATA command");
    let end = seen[start..]
        .iter()
        .position(|line| line == ".")
        .map(|offset| start + offset)
        .expect("end of data");
    seen[start + 1..end].to_vec()
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

    let message = OutgoingMessage {
        to: vec!["peer@example.com".into()],
        cc: vec!["cc@example.com".into()],
        bcc: vec!["hidden@example.com".into()],
        subject: "hello".into(),
        body_text: "body".into(),
        attachments: vec![OutgoingAttachment {
            filename: "note.txt".into(),
            content_type: Some("text/plain".into()),
            data: b"attachment body".to_vec(),
        }],
    };
    let outcome: SendOutcome = core.send("me@example.com", &message).await.expect("send");
    assert_eq!(outcome.recipients, 3);
    assert!(outcome.saved_to.is_none(), "POP3 has no Sent mailbox");
    assert!(outcome.save_error.is_none());

    let seen = server.await.expect("server");
    assert!(seen.iter().any(|line| line == "MAIL FROM:<me@example.com>"));
    for recipient in ["peer@example.com", "cc@example.com", "hidden@example.com"] {
        assert!(
            seen.iter()
                .any(|line| line == &format!("RCPT TO:<{recipient}>")),
            "missing RCPT TO for {recipient}"
        );
    }

    let data = data_lines(&seen);
    let raw = format!("{}\r\n", data.join("\r\n"));
    let parsed = mail_parse::parse_message(raw.as_bytes());
    assert_eq!(parsed.text.as_deref(), Some("body"));
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].filename.as_deref(), Some("note.txt"));
    assert_eq!(parsed.attachments[0].bytes, b"attachment body");

    let headers: Vec<&String> = data.iter().take_while(|line| !line.is_empty()).collect();
    assert!(headers.iter().any(|line| *line == "To: peer@example.com"));
    assert!(headers.iter().any(|line| *line == "Cc: cc@example.com"));
    assert!(
        headers.iter().all(|line| !line.starts_with("Bcc:")),
        "Bcc must not be written to the headers"
    );
    assert!(
        headers
            .iter()
            .all(|line| !line.to_ascii_lowercase().contains("hidden@example.com")),
        "Bcc address must not leak into the headers"
    );
}
