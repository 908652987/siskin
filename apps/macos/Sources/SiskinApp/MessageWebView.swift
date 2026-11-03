import AppKit
import SwiftUI
import WebKit

/// Sanitised HTML from `mail-parse`, rendered with JavaScript and persistent
/// storage disabled. Link clicks leave the app; embedded resources are not
/// fetched because the sanitizer strips remote URLs.
struct MessageWebView: NSViewRepresentable {
    let html: String

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    func makeNSView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.defaultWebpagePreferences.allowsContentJavaScript = false
        configuration.websiteDataStore = .nonPersistent()
        let view = WKWebView(frame: .zero, configuration: configuration)
        view.navigationDelegate = context.coordinator
        view.setValue(false, forKey: "drawsBackground")
        context.coordinator.load(html: html, into: view)
        return view
    }

    func updateNSView(_ view: WKWebView, context: Context) {
        if context.coordinator.html != html {
            context.coordinator.load(html: html, into: view)
        }
    }

    final class Coordinator: NSObject, WKNavigationDelegate {
        var html: String?

        func load(html: String, into view: WKWebView) {
            self.html = html
            let document = """
            <!doctype html><html><head><meta charset="utf-8">
            <meta name="viewport" content="width=device-width, initial-scale=1">
            <style>
            body { font: -apple-system-body; margin: 18px; color: #222; }
            img { max-width: 100%; height: auto; }
            img:not([src]), img[src=""], img[src="about:blank"] { display: none; }
            blockquote { border-left: 3px solid #ccc; margin-left: 0; padding-left: 12px; color: #555; }
            pre { white-space: pre-wrap; }
            </style></head><body>\(html)</body></html>
            """
            view.loadHTMLString(document, baseURL: nil)
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
        ) {
            guard let url = navigationAction.request.url else {
                decisionHandler(.allow)
                return
            }
            if url.scheme == "about" || url.scheme == "data" {
                decisionHandler(.allow)
                return
            }
            decisionHandler(.cancel)
            if navigationAction.navigationType == .linkActivated,
               let scheme = url.scheme?.lowercased(),
               ["http", "https", "mailto"].contains(scheme) {
                NSWorkspace.shared.open(url)
            }
        }
    }
}
