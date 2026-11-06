import Foundation
import MailFFI

enum BodyLoadState {
    case idle
    case loading
    case loaded
    case failed(String)
}

@MainActor
final class AppModel: ObservableObject {
    @Published var accounts: [AccountView] = []
    @Published var selectedEmail: String?
    @Published var mailboxes: [MailboxView] = []
    @Published var selectedMailboxId: Int64?
    @Published var messages: [MessageView] = []
    @Published var total = 0
    @Published var selectedMessageId: Int64?
    @Published var body: MessageBodyView?
    @Published var bodyState: BodyLoadState = .idle
    @Published var searchText = ""
    @Published var status = L10n.t("status.opening")
    @Published var error: String?
    @Published var busy = false
    @Published var opened = false

    private var client: MailClient?
    private var started = false
    private var searchTask: Task<Void, Never>?

    var selectedMessage: MessageView? {
        guard let selectedMessageId else { return nil }
        return messages.first { $0.id == selectedMessageId }
    }

    func start(dbPath: String) {
        guard !started else { return }
        started = true
        status = L10n.t("status.opening")
        Task {
            do {
                let client = try MailClient.open(dbPath: dbPath)
                self.client = client
                accounts = try await client.accounts()
                selectedEmail = accounts.first?.email
                await refreshMailboxes()
                await refreshMessages()
                status = accounts.isEmpty
                    ? L10n.t("status.noAccounts")
                    : L10n.t("status.messages", messages.count, total)
                log("siskin: accounts=\(accounts.count) status=\(status)")
                if let raw = ProcessInfo.processInfo.environment["SISKIN_SNAPSHOT_MESSAGE"],
                   let id = Int64(raw) {
                    selectMessage(id)
                }
            } catch {
                status = L10n.t("status.openFailed")
                self.error = describe(error)
            }
            opened = true
        }
    }

    func selectAccount(_ email: String) {
        guard email != selectedEmail else { return }
        selectedEmail = email
        selectedMailboxId = nil
        messages = []
        total = 0
        selectedMessageId = nil
        body = nil
        bodyState = .idle
        Task {
            await refreshMailboxes()
            await refreshMessages()
        }
    }

    func selectMailbox(_ id: Int64?) {
        selectedMailboxId = id
        selectedMessageId = nil
        body = nil
        bodyState = .idle
        Task { await refreshMessages() }
    }

    func updateSearch(_ text: String) {
        searchText = text
        searchTask?.cancel()
        searchTask = Task {
            try? await Task.sleep(nanoseconds: 300_000_000)
            guard !Task.isCancelled else { return }
            await refreshMessages()
        }
    }

    func selectMessage(_ id: Int64?) {
        if selectedMessageId == id, bodyState.isLoaded {
            return
        }
        selectedMessageId = id
        body = nil
        guard let id, let client, let email = selectedEmail else {
            bodyState = .idle
            return
        }
        bodyState = .loading
        Task {
            do {
                let content = try await client.body(email: email, messageId: id)
                guard selectedMessageId == id else { return }
                body = content
                bodyState = .loaded
                if let index = messages.firstIndex(where: { $0.id == id }), messages[index].unread {
                    try await client.setSeen(email: email, messageId: id, seen: true)
                    messages[index].unread = false
                    await refreshMailboxes()
                }
            } catch {
                guard selectedMessageId == id else { return }
                bodyState = .failed(describe(error))
            }
        }
    }

    func refreshMailboxes() async {
        guard let client, let email = selectedEmail else { return }
        do {
            mailboxes = try await client.mailboxes(email: email)
            if let selected = selectedMailboxId,
               !mailboxes.contains(where: { $0.id == selected }) {
                selectedMailboxId = nil
            }
        } catch {
            self.error = describe(error)
        }
    }

    func refreshMessages() async {
        guard let client, let email = selectedEmail else { return }
        do {
            let page = try await client.messages(
                email: email,
                mailboxId: selectedMailboxId,
                query: searchText.isEmpty ? nil : searchText,
                limit: 200,
                offset: 0
            )
            messages = page.messages
            total = Int(page.total)
            if let selected = selectedMessageId,
               !messages.contains(where: { $0.id == selected }) {
                selectedMessageId = nil
                body = nil
                bodyState = .idle
            }
            status = L10n.t("status.messages", messages.count, total)
            log("siskin: mailboxes=\(mailboxes.count) messages=\(messages.count) total=\(page.total)")
        } catch {
            self.error = describe(error)
        }
    }

    func loadMore() async {
        guard let client, let email = selectedEmail, messages.count < total else { return }
        busy = true
        defer { busy = false }
        do {
            let page = try await client.messages(
                email: email,
                mailboxId: selectedMailboxId,
                query: searchText.isEmpty ? nil : searchText,
                limit: 200,
                offset: UInt32(messages.count)
            )
            messages.append(contentsOf: page.messages)
            total = Int(page.total)
        } catch {
            self.error = describe(error)
        }
    }

    func syncNow() async {
        guard let client, let email = selectedEmail else { return }
        busy = true
        status = L10n.t("status.syncing")
        do {
            let summary = try await client.sync(email: email)
            status = L10n.t("status.synced", summary.inserted, summary.removed)
            if !summary.failures.isEmpty {
                error = summary.failures.joined(separator: "\n")
            }
            await refreshMailboxes()
            await refreshMessages()
        } catch {
            self.error = describe(error)
            status = L10n.t("status.syncFailed")
        }
        busy = false
    }

    func toggleRead() async {
        guard let message = selectedMessage else { return }
        await setRead(message, read: message.unread)
    }

    func setRead(_ message: MessageView, read: Bool) async {
        guard let client, let email = selectedEmail else { return }
        do {
            try await client.setSeen(email: email, messageId: message.id, seen: read)
            if let index = messages.firstIndex(where: { $0.id == message.id }) {
                messages[index].unread = !read
            }
            await refreshMailboxes()
        } catch {
            self.error = describe(error)
        }
    }

    func toggleFlag() async {
        guard let message = selectedMessage else { return }
        await setFlagged(message, flagged: !message.flagged)
    }

    func setFlagged(_ message: MessageView, flagged: Bool) async {
        guard let client, let email = selectedEmail else { return }
        do {
            try await client.setFlagged(email: email, messageId: message.id, flagged: flagged)
            if let index = messages.firstIndex(where: { $0.id == message.id }) {
                messages[index].flagged = flagged
            }
        } catch {
            self.error = describe(error)
        }
    }

    func move(_ message: MessageView, to mailboxId: Int64) async {
        guard let client, let email = selectedEmail else { return }
        do {
            try await client.moveMessage(
                email: email,
                messageId: message.id,
                mailboxId: mailboxId
            )
            await refreshMailboxes()
            await refreshMessages()
            selectedMessageId = nil
            body = nil
            bodyState = .idle
        } catch {
            self.error = describe(error)
        }
    }

    func attachment(part: String) async -> Data? {
        guard let client, let email = selectedEmail, let id = selectedMessageId else { return nil }
        do {
            return try await client.attachment(email: email, messageId: id, part: part)
        } catch {
            self.error = describe(error)
            return nil
        }
    }

    /// Sends through the core and refreshes the Sent counts. Returns the error
    /// message so the compose sheet can stay open with the draft intact.
    func send(
        to: [String],
        cc: [String],
        bcc: [String],
        subject: String,
        bodyText: String,
        attachments: [OutgoingAttachmentView]
    ) async -> String? {
        guard let client else { return L10n.t("compose.noDatabase") }
        guard let email = selectedEmail else { return L10n.t("compose.noAccount") }
        do {
            let result = try await client.send(request: SendRequestView(
                email: email,
                to: to,
                cc: cc,
                bcc: bcc,
                subject: subject,
                bodyText: bodyText,
                attachments: attachments
            ))
            status = L10n.t("status.sent", Int(result.recipients))
            if let warning = result.saveError {
                error = L10n.t("compose.notSaved", result.savedTo ?? "Sent", warning)
            }
            await refreshMailboxes()
            await refreshMessages()
            return nil
        } catch {
            return describe(error)
        }
    }

    func addAccount(
        provider: String,
        email: String,
        displayName: String?,
        host: String,
        port: UInt16,
        username: String,
        password: String,
        security: SecurityFfi,
        smtpHost: String?,
        smtpPort: UInt16?,
        smtpSecurity: SecurityFfi?,
        smtpUsername: String?
    ) async {
        guard let client else { return }
        busy = true
        do {
            let id = try await client.addAccount(
                account: NewAccountView(
                    email: email,
                    displayName: displayName,
                    provider: provider,
                    host: host,
                    port: port,
                    username: username,
                    password: password,
                    protocol: .imap,
                    security: security,
                    smtpHost: smtpHost,
                    smtpPort: smtpPort,
                    smtpSecurity: smtpSecurity,
                    smtpUsername: smtpUsername,
                    sentMailbox: nil
                )
            )
            accounts = try await client.accounts()
            status = L10n.t("status.addedAccount", id)
            selectedEmail = email
            await refreshMailboxes()
            await refreshMessages()
        } catch {
            self.error = describe(error)
        }
        busy = false
    }

    func dismissError() {
        error = nil
    }

    private func describe(_ error: Error) -> String {
        if case let FfiError.Failure(message) = error {
            return message
        }
        return error.localizedDescription
    }

    private func log(_ text: String) {
        FileHandle.standardError.write(Data((text + "\n").utf8))
    }
}

private extension BodyLoadState {
    var isLoaded: Bool {
        if case .loaded = self { return true }
        return false
    }
}
