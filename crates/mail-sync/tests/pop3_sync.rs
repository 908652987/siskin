//! POP3 mirroring: UIDL dedupe, header fetch and removal on server-side delete.

use std::sync::Arc;

use mail_proto::pop3::{Pop3Config, Pop3Session};
use mail_proto::Security;
use mail_store::{DbKey, Store};
use mail_sync::Pop3SyncEngine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn key() -> DbKey {
    DbKey::from_bytes(&[9u8; 32]).expect("key")
}

async fn serve(mut socket: TcpStream, uids: &[&str]) {
    let (read, mut write) = socket.split();
    let mut lines = BufReader::new(read).lines();
    write.write_all(b"+OK pop3\r\n").await.expect("greeting");
    while let Some(line) = lines.next_line().await.expect("read") {
        let reply = match line.as_str() {
            "CAPA" => "+OK\r\nUIDL\r\nTOP\r\n.\r\n".to_string(),
            "USER user@example.com" | "PASS secret" => "+OK\r\n".to_string(),
            "LIST" => {
                let mut reply = String::from("+OK\r\n");
                for (index, _) in uids.iter().enumerate() {
                    reply.push_str(&format!("{} {}\r\n", index + 1, 100 + index));
                }
                reply.push_str(".\r\n");
                reply
            }
            "UIDL" => {
                let mut reply = String::from("+OK\r\n");
                for (index, uid) in uids.iter().enumerate() {
                    reply.push_str(&format!("{} {uid}\r\n", index + 1));
                }
                reply.push_str(".\r\n");
                reply
            }
            command if command.starts_with("TOP ") => {
                let number: usize = command
                    .split_whitespace()
                    .nth(1)
                    .and_then(|value| value.parse().ok())
                    .expect("number");
                format!(
                    "+OK\r\nSubject: message {number}\r\nFrom: a@example.com\r\nMessage-ID: <{number}@example.com>\r\n\r\n.\r\n"
                )
            }
            "QUIT" => "+OK bye\r\n".to_string(),
            other => format!("-ERR unknown {other}\r\n"),
        };
        write.write_all(reply.as_bytes()).await.expect("reply");
        if line == "QUIT" {
            break;
        }
    }
}

#[tokio::test]
async fn syncing_mirrors_messages_and_removes_deleted_uids() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("accept 1");
        serve(socket, &["uid-a", "uid-b"]).await;
        let (socket, _) = listener.accept().await.expect("accept 2");
        serve(socket, &["uid-a"]).await;
    });

    let store = Arc::new(Store::open_in_memory(&key()).expect("store"));
    let account = store
        .create_account("test", "a@example.com", None, "ref", &serde_json::json!({}))
        .expect("account");
    let engine = Pop3SyncEngine::new(Arc::clone(&store));
    let config = Pop3Config {
        host: "127.0.0.1".into(),
        port,
        security: Security::Plaintext,
        username: "user@example.com".into(),
        password: "secret".into(),
    };

    let mut session = Pop3Session::connect(&config).await.expect("login 1");
    let first = engine.sync(account, &mut session).await.expect("sync 1");
    session.quit().await;
    assert_eq!(first.inserted, 2);

    let mut session = Pop3Session::connect(&config).await.expect("login 2");
    let second = engine.sync(account, &mut session).await.expect("sync 2");
    session.quit().await;
    assert_eq!(second.inserted, 0);
    assert_eq!(second.removed, 1);

    server.await.expect("server");
}
