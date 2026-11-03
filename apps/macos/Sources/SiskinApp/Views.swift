import AppKit
import SwiftUI
import MailFFI

extension Notification.Name {
    static let siskinSync = Notification.Name("siskin.sync")
    static let siskinReload = Notification.Name("siskin.reload")
    static let siskinFocusSearch = Notification.Name("siskin.focusSearch")
}

enum BodyMode: Hashable {
    case html
    case plain
}

struct RootView: View {
    let dbPath: String
    @StateObject private var model = AppModel()
    @State private var showAddAccount = false

    var body: some View {
        Group {
            if !model.opened {
                ProgressView(L10n.t("status.opening"))
            } else if model.accounts.isEmpty {
                AccountSetupView(model: model, isInitial: true)
                    .background(
                        Color(nsColor: .windowBackgroundColor),
                        in: RoundedRectangle(cornerRadius: 16)
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 16)
                            .strokeBorder(.separator)
                    )
                    .shadow(radius: 18, y: 6)
            } else {
                MailBrowserView(model: model, showAddAccount: $showAddAccount)
            }
        }
        .frame(minWidth: 900, minHeight: 720)
        .background(Color(nsColor: .windowBackgroundColor))
        .task { model.start(dbPath: dbPath) }
        .onAppear {
            if ProcessInfo.processInfo.environment["SISKIN_SNAPSHOT_ADD_ACCOUNT"] != nil {
                showAddAccount = true
            }
        }
        .sheet(isPresented: $showAddAccount) {
            AccountSetupView(model: model, isInitial: false)
        }
        .alert(
            L10n.t("error.title"),
            isPresented: Binding(
                get: { model.error != nil },
                set: { if !$0 { model.dismissError() } }
            )
        ) {
            Button(L10n.t("action.ok")) { model.dismissError() }
        } message: {
            Text(model.error ?? "")
        }
        .onReceive(NotificationCenter.default.publisher(for: .siskinSync)) { _ in
            Task { await model.syncNow() }
        }
        .onReceive(NotificationCenter.default.publisher(for: .siskinReload)) { _ in
            Task {
                await model.refreshMailboxes()
                await model.refreshMessages()
            }
        }
    }
}

private struct MailBrowserView: View {
    @ObservedObject var model: AppModel
    @Binding var showAddAccount: Bool
    @FocusState private var searchFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            topBar
            Divider()
            HSplitView {
                sidebar
                    .frame(minWidth: 190, idealWidth: 220, maxWidth: 300, maxHeight: .infinity)
                messageList
                    .frame(minWidth: 300, idealWidth: 400, maxWidth: 560, maxHeight: .infinity)
                detail
                    .frame(minWidth: 360, maxHeight: .infinity)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            Divider()
            statusBar
        }
        .onReceive(NotificationCenter.default.publisher(for: .siskinFocusSearch)) { _ in
            searchFocused = true
        }
    }

    private var topBar: some View {
        HStack(spacing: 10) {
            Picker(
                L10n.t("account.label"),
                selection: Binding(
                    get: { model.selectedEmail ?? "" },
                    set: { value in
                        DispatchQueue.main.async { model.selectAccount(value) }
                    }
                )
            ) {
                ForEach(model.accounts, id: \.email) { account in
                    Text(account.displayName ?? account.email).tag(account.email)
                }
            }
            .labelsHidden()
            .frame(width: 220)

            Spacer(minLength: 12)

            HStack(spacing: 4) {
                Image(systemName: "magnifyingglass")
                    .foregroundStyle(.secondary)
                TextField(L10n.t("search.placeholder"), text: Binding(
                    get: { model.searchText },
                    set: { model.updateSearch($0) }
                ))
                .textFieldStyle(.plain)
                .focused($searchFocused)
                if !model.searchText.isEmpty {
                    Button {
                        model.updateSearch("")
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .background(.quaternary, in: RoundedRectangle(cornerRadius: 7))
            .frame(width: 300)

            Button {
                Task { await model.syncNow() }
            } label: {
                Label(L10n.t("action.sync"), systemImage: "arrow.clockwise")
            }
            .disabled(model.busy)

            Button {
                showAddAccount = true
            } label: {
                Label(L10n.t("action.addAccount"), systemImage: "plus")
            }
            .keyboardShortcut("n", modifiers: [.command, .shift])
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }

    private var sidebar: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                SidebarRow(
                    title: L10n.t("sidebar.allMail"),
                    systemImage: "tray.full",
                    unread: model.mailboxes.reduce(0) { $0 + $1.unread },
                    selected: model.selectedMailboxId == nil
                ) {
                    model.selectMailbox(nil)
                }
                ForEach(model.mailboxes, id: \.id) { mailbox in
                    SidebarRow(
                        title: displayMailbox(mailbox),
                        systemImage: icon(for: mailbox.role),
                        unread: mailbox.unread,
                        selected: model.selectedMailboxId == mailbox.id
                    ) {
                        model.selectMailbox(mailbox.id)
                    }
                }
            }
            .padding(8)
        }
        .background(.ultraThinMaterial)
    }

    private var messageList: some View {
        ScrollView {
            LazyVStack(spacing: 0) {
                ForEach(model.messages, id: \.id) { message in
                    MessageRow(message: message)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 6)
                        .background(
                            message.id == model.selectedMessageId
                                ? Color.accentColor.opacity(0.18)
                                : Color.clear
                        )
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.selectMessage(message.id)
                        }
                        .contextMenu {
                            MessageActions(message: message, model: model)
                        }
                    Divider()
                        .padding(.leading, 12)
                }
                if model.messages.count < model.total {
                    Button {
                        Task { await model.loadMore() }
                    } label: {
                        Text(L10n.t("action.loadMore"))
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.plain)
                    .padding(.vertical, 8)
                }
            }
        }
        .background(Color(nsColor: .controlBackgroundColor))
        .overlay {
            if model.messages.isEmpty {
                ContentUnavailableView(
                    L10n.t("list.empty.title"),
                    systemImage: "tray",
                    description: Text(L10n.t("list.empty.description"))
                )
            }
        }
    }

    @ViewBuilder
    private var detail: some View {
        if let message = model.selectedMessage {
            MessageDetailView(
                model: model,
                message: message,
                attachments: model.body?.attachments ?? []
            )
        } else {
            ContentUnavailableView(
                L10n.t("detail.empty.title"),
                systemImage: "envelope",
                description: Text(L10n.t("detail.empty.description"))
            )
        }
    }

    private var statusBar: some View {
        HStack {
            Text(model.status)
            Spacer()
            if model.busy {
                ProgressView()
                    .controlSize(.small)
            }
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.vertical, 5)
    }

    private func icon(for role: String?) -> String {
        switch role {
        case "inbox": return "tray"
        case "sent": return "paperplane"
        case "drafts": return "square.and.pencil"
        case "trash": return "trash"
        case "junk": return "exclamationmark.triangle"
        case "archive": return "archivebox"
        default: return "folder"
        }
    }
}

private struct MessageDetailView: View {
    @ObservedObject var model: AppModel
    let message: MessageView
    let attachments: [AttachmentView]
    @State private var mode: BodyMode = .html

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if model.body?.html != nil {
                HStack {
                    Spacer()
                    Picker(L10n.t("detail.view"), selection: $mode) {
                        Text(L10n.t("detail.html")).tag(BodyMode.html)
                        Text(L10n.t("detail.plain")).tag(BodyMode.plain)
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .frame(width: 170)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 6)
                Divider()
            }
            bodyContent
            if !attachments.isEmpty {
                Divider()
                attachmentList
            }
        }
        .onChange(of: model.selectedMessageId) { _, _ in
            DispatchQueue.main.async {
                mode = model.body?.html == nil ? .plain : .html
            }
        }
        .onChange(of: model.body?.html != nil) { _, hasHTML in
            if hasHTML { mode = .html }
        }
        .onAppear {
            mode = model.body?.html == nil ? .plain : .html
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(message.subject)
                    .font(.title2)
                    .textSelection(.enabled)
                Spacer()
                if message.flagged {
                    Image(systemName: "star.fill")
                        .foregroundStyle(.yellow)
                }
            }
            HStack {
                Text(displaySender(message.from))
                Spacer()
                Text(dateText(message.sentAt))
            }
            .font(.callout)
            .foregroundStyle(.secondary)
            HStack(spacing: 8) {
                Button(message.unread ? L10n.t("action.markRead") : L10n.t("action.markUnread")) {
                    Task { await model.toggleRead() }
                }
                .fixedSize()
                Button(message.flagged ? L10n.t("action.unflag") : L10n.t("action.flag")) {
                    Task { await model.toggleFlag() }
                }
                .fixedSize()
                Menu(L10n.t("action.moveTo")) {
                    ForEach(model.mailboxes.filter { $0.id != message.mailboxId }, id: \.id) { mailbox in
                        Button(displayMailbox(mailbox)) {
                            Task { await model.move(message, to: mailbox.id) }
                        }
                    }
                }
                .fixedSize()
                Spacer(minLength: 8)
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(16)
    }

    @ViewBuilder
    private var bodyContent: some View {
        switch model.bodyState {
        case .idle:
            ContentUnavailableView(
                L10n.t("detail.empty.title"),
                systemImage: "envelope",
                description: Text(L10n.t("detail.empty.description"))
            )
        case .loading:
            ProgressView(L10n.t("detail.loading"))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            Text(message)
                .foregroundStyle(.secondary)
                .padding(18)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        case .loaded:
            if mode == .html, let html = model.body?.html {
                MessageWebView(html: html)
            } else {
                ScrollView {
                    Text(model.body?.text ?? L10n.t("detail.noBody"))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(18)
                }
            }
        }
    }

    private var attachmentList: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(L10n.t("detail.attachments"))
                .font(.headline)
            ForEach(attachments, id: \.part) { attachment in
                HStack {
                    Image(systemName: "paperclip")
                    Text(attachment.filename ?? attachment.contentType)
                    Spacer()
                    Text(byteText(attachment.size))
                        .foregroundStyle(.secondary)
                    Button(L10n.t("action.save")) {
                        save(attachment)
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                }
            }
        }
        .padding(12)
    }

    private func save(_ attachment: AttachmentView) {
        Task {
            guard let data = await model.attachment(part: attachment.part) else { return }
            let panel = NSSavePanel()
            panel.title = L10n.t("attachment.saveTitle")
            panel.nameFieldStringValue = attachment.filename ?? "attachment"
            guard panel.runModal() == .OK, let url = panel.url else { return }
            do {
                try data.write(to: url)
            } catch {
                model.error = error.localizedDescription
            }
        }
    }
}

private struct MessageActions: View {
    let message: MessageView
    @ObservedObject var model: AppModel

    var body: some View {
        Button(message.unread ? L10n.t("action.markRead") : L10n.t("action.markUnread")) {
            Task { await model.setRead(message, read: message.unread) }
        }
        Button(message.flagged ? L10n.t("action.unflag") : L10n.t("action.flag")) {
            Task { await model.setFlagged(message, flagged: !message.flagged) }
        }
        Menu(L10n.t("action.moveTo")) {
            ForEach(model.mailboxes.filter { $0.id != message.mailboxId }, id: \.id) { mailbox in
                Button(displayMailbox(mailbox)) {
                    Task { await model.move(message, to: mailbox.id) }
                }
            }
        }
    }
}

private struct SidebarRow: View {
    let title: String
    let systemImage: String
    let unread: Int64
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: systemImage)
                    .frame(width: 16)
                Text(title)
                Spacer(minLength: 4)
                if unread > 0 {
                    Text("\(unread)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: 6)
                    .fill(selected ? Color.accentColor.opacity(0.22) : Color.clear)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

private struct MessageRow: View {
    let message: MessageView

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                if message.unread {
                    Circle()
                        .fill(Color.accentColor)
                        .frame(width: 7, height: 7)
                }
                Text(displaySender(message.from))
                    .font(.callout)
                    .lineLimit(1)
                Spacer()
                if message.flagged {
                    Image(systemName: "star.fill")
                        .font(.caption)
                        .foregroundStyle(.yellow)
                }
                Text(shortDate(message.sentAt))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Text(message.subject.isEmpty ? L10n.t("detail.noSubject") : message.subject)
                .font(.body.weight(message.unread ? .semibold : .regular))
                .lineLimit(1)
            Text(message.snippet)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(2)
        }
        .padding(.vertical, 3)
    }
}

private struct AccountSetupView: View {
    @ObservedObject var model: AppModel
    let isInitial: Bool
    @Environment(\.dismiss) private var dismiss

    @State private var email = ""
    @State private var displayName = ""
    @State private var host = ""
    @State private var port = "993"
    @State private var username = ""
    @State private var password = ""
    @State private var security: SecurityFfi = .tls
    @State private var provider = "custom"
    @State private var usernameEdited = false
    @FocusState private var emailFocused: Bool

    private var validPort: UInt16? { UInt16(port) }
    private var portInvalid: Bool { !port.isEmpty && validPort == nil }
    private var canSubmit: Bool {
        !email.trimmingCharacters(in: .whitespaces).isEmpty
            && !host.trimmingCharacters(in: .whitespaces).isEmpty
            && !password.isEmpty
            && validPort != nil
            && !model.busy
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    presetSection
                    accountSection
                    serverSection
                }
                .padding(24)
            }
            Divider()
            footer
        }
        .frame(width: 560, height: 720)
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear {
            emailFocused = true
            let environment = ProcessInfo.processInfo.environment
            if let presetEmail = environment["SISKIN_PRESET_EMAIL"] {
                email = presetEmail
                if !usernameEdited {
                    username = presetEmail
                }
            }
            if let preset = environment["SISKIN_PRESET"] {
                applyPreset(preset)
            }
        }
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 14) {
            ZStack {
                Circle()
                    .fill(Color.accentColor.opacity(0.15))
                    .frame(width: 44, height: 44)
                Image(systemName: "envelope.badge")
                    .font(.title3)
                    .foregroundStyle(.tint)
            }
            VStack(alignment: .leading, spacing: 4) {
                Text(L10n.t("account.title"))
                    .font(.title2.weight(.semibold))
                Text(L10n.t("account.subtitle"))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
        }
        .padding(24)
    }

    private var presetSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(L10n.t("account.presets"))
                .font(.headline)
            HStack(spacing: 8) {
                ForEach(["gmail", "qq", "88"], id: \.self) { id in
                    Button {
                        applyPreset(id)
                    } label: {
                        HStack(spacing: 5) {
                            Text(L10n.t("provider.\(id)"))
                            if provider == id {
                                Image(systemName: "checkmark")
                            }
                        }
                    }
                    .buttonStyle(.bordered)
                    .tint(provider == id ? .accentColor : .secondary)
                }
                Button {
                    provider = "custom"
                    host = ""
                    port = "993"
                    security = .tls
                } label: {
                    HStack(spacing: 5) {
                        Text(L10n.t("provider.custom"))
                        if provider == "custom" {
                            Image(systemName: "checkmark")
                        }
                    }
                }
                .buttonStyle(.bordered)
                .tint(provider == "custom" ? .accentColor : .secondary)
                Spacer()
            }
            Text(providerHint)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .onChange(of: email) { _, newValue in
            if !usernameEdited {
                username = newValue.trimmingCharacters(in: .whitespaces)
            }
            if provider == "88" {
                applyPreset("88")
            }
        }
        .onChange(of: security) { old, new in
            if old != new, port == "993" || port == "143" {
                port = new == .tls ? "993" : "143"
            }
        }
    }

    private var providerHint: String {
        switch provider {
        case "gmail": return L10n.t("account.hint.gmail")
        case "qq": return L10n.t("account.hint.qq")
        case "88": return L10n.t("account.hint.88")
        default: return L10n.t("account.hint.custom")
        }
    }

    private func applyPreset(_ id: String) {
        let trimmed = email.trimmingCharacters(in: .whitespaces)
        let presets = providerPresets(email: trimmed.isEmpty ? nil : trimmed)
        guard let preset = presets.first(where: { $0.id == id }) else { return }
        provider = preset.id
        host = preset.host
        port = String(preset.port)
        security = preset.security
    }

    private var accountSection: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(L10n.t("account.section.account"))
                .font(.headline)
            VStack(alignment: .leading, spacing: 6) {
                Text(L10n.t("account.email"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                TextField(L10n.t("account.email.placeholder"), text: $email)
                    .textFieldStyle(.roundedBorder)
                    .controlSize(.large)
                    .focused($emailFocused)
            }
            labeledField(
                L10n.t("account.displayNameOptional"),
                text: $displayName,
                placeholder: L10n.t("account.displayName.placeholder")
            )
        }
    }

    private var serverSection: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(L10n.t("account.section.server"))
                .font(.headline)
            HStack(alignment: .top, spacing: 12) {
                labeledField(
                    L10n.t("account.host"),
                    text: $host,
                    placeholder: L10n.t("account.host.placeholder")
                )
                .frame(maxWidth: .infinity)
                VStack(alignment: .leading, spacing: 6) {
                    Text(L10n.t("account.port"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    TextField("993", text: $port)
                        .textFieldStyle(.roundedBorder)
                        .controlSize(.large)
                        .frame(width: 96)
                    if portInvalid {
                        Text(L10n.t("account.port.invalid"))
                            .font(.caption2)
                            .foregroundStyle(.red)
                    }
                }
            }
            VStack(alignment: .leading, spacing: 6) {
                Text(L10n.t("account.security"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Picker("", selection: $security) {
                    Text(L10n.t("account.security.tls")).tag(SecurityFfi.tls)
                    Text(L10n.t("account.security.starttls")).tag(SecurityFfi.startTls)
                    Text(L10n.t("account.security.plaintext")).tag(SecurityFfi.plaintext)
                }
                .labelsHidden()
                .pickerStyle(.menu)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Text(L10n.t("account.username"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    if usernameEdited {
                        Button(L10n.t("account.useEmail")) {
                            username = email.trimmingCharacters(in: .whitespaces)
                            usernameEdited = false
                        }
                        .buttonStyle(.link)
                        .font(.caption)
                    }
                }
                TextField(L10n.t("account.username.placeholder"), text: $username)
                    .textFieldStyle(.roundedBorder)
                    .controlSize(.large)
                    .onChange(of: username) { _, newValue in
                        usernameEdited = newValue.trimmingCharacters(in: .whitespaces)
                            != email.trimmingCharacters(in: .whitespaces)
                    }
            }
            VStack(alignment: .leading, spacing: 6) {
                Text(L10n.t("account.password"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                SecureField(L10n.t("account.password.placeholder"), text: $password)
                    .textFieldStyle(.roundedBorder)
                    .controlSize(.large)
                Text(L10n.t("account.passwordHelp"))
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func labeledField(
        _ label: String,
        text: Binding<String>,
        placeholder: String
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
            TextField(placeholder, text: text)
                .textFieldStyle(.roundedBorder)
                .controlSize(.large)
        }
    }

    private var footer: some View {
        HStack(spacing: 12) {
            if !isInitial {
                Button(L10n.t("action.cancel")) { dismiss() }
                    .keyboardShortcut(.cancelAction)
            }
            Spacer()
            if model.busy {
                ProgressView()
                    .controlSize(.small)
            }
            Button(isInitial ? L10n.t("action.add") : L10n.t("action.addAccount")) {
                submit()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(!canSubmit)
        }
        .padding(.horizontal, 24)
        .padding(.vertical, 16)
    }

    private func submit() {
        guard let portNumber = validPort else { return }
        Task {
            await model.addAccount(
                provider: provider,
                email: email.trimmingCharacters(in: .whitespaces),
                displayName: displayName.trimmingCharacters(in: .whitespaces).isEmpty
                    ? nil
                    : displayName.trimmingCharacters(in: .whitespaces),
                host: host.trimmingCharacters(in: .whitespaces),
                port: portNumber,
                username: username.trimmingCharacters(in: .whitespaces).isEmpty
                    ? email.trimmingCharacters(in: .whitespaces)
                    : username.trimmingCharacters(in: .whitespaces),
                password: password,
                security: security
            )
            if model.error == nil && !isInitial {
                dismiss()
            }
        }
    }
}

private func displayMailbox(_ mailbox: MailboxView) -> String {
    switch mailbox.role {
    case "inbox": return L10n.t("mailbox.inbox")
    case "sent": return L10n.t("mailbox.sent")
    case "drafts": return L10n.t("mailbox.drafts")
    case "trash": return L10n.t("mailbox.trash")
    case "junk", "spam": return L10n.t("mailbox.junk")
    case "archive": return L10n.t("mailbox.archive")
    default: return mailbox.name
    }
}

private func displaySender(_ raw: String) -> String {
    if raw.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
        return L10n.t("detail.unknownSender")
    }
    guard let start = raw.lastIndex(of: "<"), let end = raw.lastIndex(of: ">"), start < end else {
        return raw
    }
    let name = raw[..<start]
        .trimmingCharacters(in: .whitespacesAndNewlines)
        .trimmingCharacters(in: CharacterSet(charactersIn: "\""))
    if !name.isEmpty {
        return name
    }
    return String(raw[raw.index(after: start)..<end])
}

private func dateText(_ timestamp: Int64?) -> String {
    guard let timestamp else { return "" }
    return Date(timeIntervalSince1970: TimeInterval(timestamp))
        .formatted(date: .abbreviated, time: .shortened)
}

private func shortDate(_ timestamp: Int64?) -> String {
    guard let timestamp else { return "" }
    return Date(timeIntervalSince1970: TimeInterval(timestamp))
        .formatted(date: .abbreviated, time: .omitted)
}

private func byteText(_ bytes: Int64) -> String {
    ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
}
