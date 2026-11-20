// Run the browser compatibility page in the installed system WebKit engine.
// This avoids changing Safari's global remote-automation preference.
import Cocoa
import WebKit

final class Probe: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
    var webView: WKWebView!
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let result = message.body as? [String: Any] else { fputs("Unexpected WebKit result: \(message.body)\n", stderr); exit(1) }
        if let data = try? JSONSerialization.data(withJSONObject: result, options: [.sortedKeys]),
           let text = String(data: data, encoding: .utf8) {
            print(text)
        }
        exit(result["error"] == nil ? 0 : 1)
    }
    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        fputs("\(error)\n", stderr); exit(1)
    }
}
let application = NSApplication.shared
application.setActivationPolicy(.prohibited)
let probe = Probe()
let configuration = WKWebViewConfiguration()
configuration.websiteDataStore = .nonPersistent()
configuration.userContentController.add(probe, name: "result")
configuration.userContentController.addUserScript(WKUserScript(source: "window.addEventListener('error', event => window.webkit.messageHandlers.result.postMessage({error:event.message}));", injectionTime: .atDocumentStart, forMainFrameOnly: true))
probe.webView = WKWebView(frame: .zero, configuration: configuration)
probe.webView.navigationDelegate = probe
probe.webView.load(URLRequest(url: URL(string: CommandLine.arguments[1])!))
DispatchQueue.main.asyncAfter(deadline: .now() + 60) { fputs("WebKit probe timed out\n", stderr); exit(1) }
application.run()
