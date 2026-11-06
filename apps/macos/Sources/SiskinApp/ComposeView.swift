import AppKit
import SwiftUI
import UniformTypeIdentifiers
import MailFFI

struct ComposeAttachment: Identifiable {
    let id = UUID()
    let filename: String
    let contentType: String?
    let data: Data
}

/// New-message sheet. The draft stays here until the core confirms the SMTP
/// submission; failures keep the sheet open with the text intact.
struct ComposeView: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    @State private var to = ""
    @State private var cc = ""
    @State private var bcc = ""
    @State private var subject = ""
    @State private var bodyText = ""
    @State private var attachments: [ComposeAttachment] = []
    @State private var showCcBcc = false
    @State private var sending = false
    @State private var failure: String?

    private var recipients: [String] { splitAddresses(to) }
    private var canSend: Bool {
        !recipients.isEmpty && model.selectedEmail != nil && !sending
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            fields
            Divider()
            editor
            if !attachments.isEmpty {
                Divider()
                attachmentList
            }
            if let failure {
                Divider()
                Text(failure)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 8)
            }
            Divider()
            footer
        }
        .frame(width: 680, height: 560)
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear { prefill() }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(L10n.t("compose.title"))
                .font(.title2.weight(.semibold))
            Text(model.selectedEmail ?? "")
                .font(.callout)
                .foregroundStyle(.secondary)
            Spacer()
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var fields: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text(L10n.t("compose.to"))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .frame(width: 64, alignment: .trailing)
                TextField("peer@example.com", text: $to)
                    .textFieldStyle(.plain)
                if !showCcBcc {
                    Button(L10n.t("compose.addCcBcc")) {
                        showCcBcc = true
                    }
                    .buttonStyle(.link)
                    .font(.caption)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            Divider()
            if showCcBcc {
                fieldRow(L10n.t("compose.cc"), text: $cc)
                Divider()
                fieldRow(L10n.t("compose.bcc"), text: $bcc)
                Divider()
            }
            fieldRow(L10n.t("compose.subject"), text: $subject)
        }
    }

    private func fieldRow(_ label: String, text: Binding<String>) -> some View {
        HStack(spacing: 10) {
            Text(label)
                .font(.callout)
                .foregroundStyle(.secondary)
                .frame(width: 64, alignment: .trailing)
            TextField("", text: text)
                .textFieldStyle(.plain)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
    }

    private var editor: some View {
        TextEditor(text: $bodyText)
            .font(.body)
            .scrollContentBackground(.hidden)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var attachmentList: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(attachments) { attachment in
                    HStack(spacing: 6) {
                        Image(systemName: "paperclip")
                        Text(attachment.filename)
                            .lineLimit(1)
                        Text(ByteCountFormatter.string(
                            fromByteCount: Int64(attachment.data.count),
                            countStyle: .file
                        ))
                        .foregroundStyle(.secondary)
                        Button {
                            attachments.removeAll { $0.id == attachment.id }
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                    }
                    .font(.caption)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
                    .background(.quaternary, in: RoundedRectangle(cornerRadius: 6))
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Button {
                pickAttachments()
            } label: {
                Label(L10n.t("compose.attach"), systemImage: "paperclip")
            }
            .disabled(sending)
            if let total = attachmentBytes {
                Text(ByteCountFormatter.string(fromByteCount: Int64(total), countStyle: .file))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if sending {
                ProgressView()
                    .controlSize(.small)
            }
            Button(L10n.t("action.cancel")) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button(L10n.t("compose.send")) {
                submit()
            }
            .buttonStyle(.borderedProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(!canSend)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private var attachmentBytes: Int? {
        attachments.isEmpty ? nil : attachments.reduce(0) { $0 + $1.data.count }
    }

    private func pickAttachments() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.title = L10n.t("compose.attach")
        guard panel.runModal() == .OK else { return }
        for url in panel.urls {
            do {
                let data = try Data(contentsOf: url)
                attachments.append(ComposeAttachment(
                    filename: url.lastPathComponent,
                    contentType: UTType(filenameExtension: url.pathExtension)?.preferredMIMEType,
                    data: data
                ))
                failure = nil
            } catch {
                failure = error.localizedDescription
            }
        }
    }

    private func submit() {
        guard canSend else { return }
        sending = true
        failure = nil
        Task {
            let error = await model.send(
                to: recipients,
                cc: splitAddresses(cc),
                bcc: splitAddresses(bcc),
                subject: subject,
                bodyText: bodyText,
                attachments: attachments.map {
                    OutgoingAttachmentView(
                        filename: $0.filename,
                        contentType: $0.contentType,
                        data: $0.data
                    )
                }
            )
            sending = false
            if let error {
                failure = error
            } else {
                dismiss()
            }
        }
    }

    /// Snapshot and smoke hook: prefill the sheet from the environment.
    private func prefill() {
        let environment = ProcessInfo.processInfo.environment
        if let value = environment["SISKIN_COMPOSE_TO"] { to = value }
        if let value = environment["SISKIN_COMPOSE_CC"] {
            cc = value
            showCcBcc = true
        }
        if let value = environment["SISKIN_COMPOSE_BCC"] {
            bcc = value
            showCcBcc = true
        }
        if let value = environment["SISKIN_COMPOSE_SUBJECT"] { subject = value }
        if let value = environment["SISKIN_COMPOSE_BODY"] { bodyText = value }
        if let path = environment["SISKIN_COMPOSE_ATTACH"] {
            let url = URL(fileURLWithPath: path)
            if let data = try? Data(contentsOf: url) {
                attachments.append(ComposeAttachment(
                    filename: url.lastPathComponent,
                    contentType: UTType(filenameExtension: url.pathExtension)?.preferredMIMEType,
                    data: data
                ))
            }
        }
    }
}

private func splitAddresses(_ raw: String) -> [String] {
    raw.split(whereSeparator: { $0 == "," || $0 == ";" })
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        .filter { !$0.isEmpty }
}
