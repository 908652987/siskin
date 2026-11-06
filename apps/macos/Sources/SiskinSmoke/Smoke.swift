import Foundation
import MailFFI

@main
struct Smoke {
    static func main() async {
        let path = ProcessInfo.processInfo.environment["SISKIN_DB"] ?? "/tmp/siskin-ui.db"
        do {
            let client = try MailClient.open(dbPath: path)
            let accounts = try await client.accounts()
            print("accounts=\(accounts.count)")
            var totalMessages: Int64 = 0
            for account in accounts {
                let mailboxes = try await client.mailboxes(email: account.email)
                let page = try await client.messages(
                    email: account.email,
                    mailboxId: nil,
                    query: nil,
                    limit: 20,
                    offset: 0
                )
                totalMessages += page.total
                print("account=\(account.id) mailboxes=\(mailboxes.count) messages=\(page.total)")
                if let first = page.messages.first {
                    let body = try await client.body(email: account.email, messageId: first.id)
                    print("bodyText=\(body.text?.count ?? 0) bodyHTML=\(body.html?.count ?? 0) attachments=\(body.attachments.count)")
                    if let attachment = body.attachments.first {
                        let data = try await client.attachment(
                            email: account.email,
                            messageId: first.id,
                            part: attachment.part
                        )
                        print("attachmentBytes=\(data.count)")
                    }
                    try await client.setSeen(email: account.email, messageId: first.id, seen: !first.unread)
                    try await client.setSeen(email: account.email, messageId: first.id, seen: first.unread)
                    try await client.setFlagged(email: account.email, messageId: first.id, flagged: !first.flagged)
                    try await client.setFlagged(email: account.email, messageId: first.id, flagged: first.flagged)
                    print("keywordRoundTrip=ok")
                }
                if ProcessInfo.processInfo.environment["SISKIN_SCAN"] != nil {
                    for message in page.messages.prefix(12) {
                        let body = try await client.body(email: account.email, messageId: message.id)
                        print("id=\(message.id) text=\(body.text?.count ?? 0) html=\(body.html?.count ?? 0) attach=\(body.attachments.count)")
                        if let attachment = body.attachments.first {
                            let data = try await client.attachment(
                                email: account.email,
                                messageId: message.id,
                                part: attachment.part
                            )
                            print("attachmentBytes=\(data.count)")
                        }
                    }
                }
            }
            if let recipient = ProcessInfo.processInfo.environment["SISKIN_SEND_TO"],
               !recipient.isEmpty {
                let environment = ProcessInfo.processInfo.environment
                let to = recipient.split(separator: ",").map {
                    $0.trimmingCharacters(in: .whitespaces)
                }
                let cc = (environment["SISKIN_SEND_CC"] ?? "")
                    .split(separator: ",")
                    .map { $0.trimmingCharacters(in: .whitespaces) }
                    .filter { !$0.isEmpty }
                let bcc = (environment["SISKIN_SEND_BCC"] ?? "")
                    .split(separator: ",")
                    .map { $0.trimmingCharacters(in: .whitespaces) }
                    .filter { !$0.isEmpty }
                var attachments: [OutgoingAttachmentView] = []
                if let path = environment["SISKIN_SEND_ATTACH"], !path.isEmpty {
                    let url = URL(fileURLWithPath: path)
                    let data = try Data(contentsOf: url)
                    attachments.append(OutgoingAttachmentView(
                        filename: url.lastPathComponent,
                        contentType: "text/plain",
                        data: data
                    ))
                }
                let result = try await client.send(request: SendRequestView(
                    email: accounts.first?.email ?? "",
                    to: to,
                    cc: cc,
                    bcc: bcc,
                    subject: environment["SISKIN_SEND_SUBJECT"] ?? "smoke",
                    bodyText: environment["SISKIN_SEND_BODY"] ?? "smoke body",
                    attachments: attachments
                ))
                print(
                    "sendRecipients=\(result.recipients) "
                        + "savedTo=\(result.savedTo ?? "-") "
                        + "saveError=\(result.saveError ?? "-")"
                )
            }
            print("totalMessages=\(totalMessages)")
            exit(0)
        } catch {
            print("smoke failed: \(error)")
            exit(1)
        }
    }
}
