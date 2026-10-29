//! Protocol-level tests against in-process mock servers.

use std::future::Future;
use std::net::SocketAddr;

use mail_proto::pop3::{Pop3Config, Pop3Session};
use mail_proto::smtp::{SmtpConfig, SmtpSession};
use mail_proto::Security;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

struct Mock {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<Vec<String>>,
}

impl Mock {
    async fn start<F, Fut>(script: F) -> Self
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = Vec<String>> + Send,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            script(socket).await
        });
        Self { addr, handle }
    }
}

#[tokio::test]
async fn pop3_lists_messages_and_reads_headers() {
    let mock = Mock::start(|mut socket| async move {
        let (read, mut write) = socket.split();
        let mut lines = BufReader::new(read).lines();
        let mut seen = Vec::new();
        write.write_all(b"+OK mock pop3\r\n").await.expect("greeting");
        while let Some(line) = lines.next_line().await.expect("read") {
            seen.push(line.clone());
            let reply = match line.as_str() {
                "CAPA" => "+OK\r\nSTLS\r\nUIDL\r\nTOP\r\n.\r\n".to_string(),
                "USER user@example.com" | "PASS secret" => "+OK\r\n".to_string(),
                "LIST" => "+OK\r\n1 120\r\n2 80\r\n.\r\n".to_string(),
                "UIDL" => "+OK\r\n1 uid-a\r\n2 uid-b\r\n.\r\n".to_string(),
                "TOP 1 0" => {
                    "+OK\r\nSubject: one\r\nFrom: a@example.com\r\nMessage-ID: <one@example.com>\r\nDate: Tue, 01 Oct 2024 10:00:00 +0800\r\n\r\n.\r\n".to_string()
                }
                "TOP 2 0" => "+OK\r\nSubject: two\r\n\r\n.\r\n".to_string(),
                "RETR 2" => {
                    "+OK\r\nSubject: two\r\n\r\nline one\r\n..dotted\r\n.\r\n".to_string()
                }
                "QUIT" => "+OK bye\r\n".to_string(),
                other => format!("-ERR unknown {other}\r\n"),
            };
            write.write_all(reply.as_bytes()).await.expect("reply");
            if line == "QUIT" {
                break;
            }
        }
        seen
    })
    .await;

    let mut session = Pop3Session::connect(&Pop3Config {
        host: mock.addr.ip().to_string(),
        port: mock.addr.port(),
        security: Security::Plaintext,
        username: "user@example.com".into(),
        password: "secret".into(),
    })
    .await
    .expect("pop3 login");

    let messages = session.list().await.expect("list");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].uid, "uid-a");
    assert_eq!(messages[0].size, 120);
    let headers = session.headers(1).await.expect("headers");
    assert!(headers.contains("Subject: one"));
    let raw = session.retrieve(2).await.expect("retr");
    assert_eq!(
        String::from_utf8(raw).expect("utf8"),
        "Subject: two\r\n\r\nline one\r\n.dotted\r\n"
    );
    session.quit().await;

    let seen = mock.handle.await.expect("mock");
    assert!(seen.iter().any(|line| line == "USER user@example.com"));
}

#[tokio::test]
async fn smtp_authenticates_and_dot_stuffs_the_message() {
    let mock = Mock::start(|mut socket| async move {
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
                "250-mock\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n".to_string()
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
    })
    .await;

    let mut session = SmtpSession::connect(&SmtpConfig {
        host: mock.addr.ip().to_string(),
        port: mock.addr.port(),
        security: Security::Plaintext,
        username: Some("user@example.com".into()),
        password: Some("secret".into()),
    })
    .await
    .expect("smtp login");

    let message = b"Subject: test\r\n\r\n.first\r\nlast\r\n";
    session
        .send("user@example.com", &["peer@example.com".into()], message)
        .await
        .expect("send");
    session.quit().await;

    let seen = mock.handle.await.expect("mock");
    assert!(seen.iter().any(|line| line.starts_with("AUTH PLAIN ")));
    assert!(seen.iter().any(|line| line == "..first"));
    assert!(seen
        .iter()
        .any(|line| line == "MAIL FROM:<user@example.com>"));
    assert!(seen.iter().any(|line| line == "RCPT TO:<peer@example.com>"));
}
