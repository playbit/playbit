import UIKit
import os

final class LogsController: UITableViewController {
    private static let performance = OSLog(subsystem: "app.playbit", category: "GuestLogs")
    private let instance: String
    private var snapshot: LogSnapshot?
    private var pending = false
    private var changed = false
    private var status: String?
    private let message = UILabel()

    init(instance: String) { self.instance = instance; super.init(style: .plain) }
    required init?(coder: NSCoder) { fatalError() }
    deinit { NotificationCenter.default.removeObserver(self) }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Logs"
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: "log-line")
        tableView.separatorStyle = .none
        tableView.estimatedRowHeight = 36
        message.numberOfLines = 0; message.textAlignment = .center
        message.font = .preferredFont(forTextStyle: .body); message.adjustsFontForContentSizeCategory = true
        message.textColor = .secondaryLabel
        tableView.backgroundView = message
        navigationItem.rightBarButtonItem = UIBarButtonItem(title: "Copy", style: .plain, target: self, action: #selector(copyLogs))
        navigationItem.rightBarButtonItem?.isEnabled = false
        NotificationCenter.default.addObserver(self, selector: #selector(localChanged), name: EngineHost.backgroundChanged, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(refresh), name: UIApplication.didBecomeActiveNotification, object: nil)
    }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); refresh() }
    private var visible: Bool {
        navigationController?.topViewController === self && UIApplication.shared.applicationState == .active
    }
    @objc private func localChanged(_ notification: Notification) {
        guard visible else { return }
        if let error = notification.userInfo?["error"] as? String { showError(error); return }
        refresh()
    }
    @objc private func refresh() {
        guard visible else { return }
        guard !pending else { changed = true; return }
        pending = true
        if snapshot == nil { message.text = "Loading logs…" }
        // This is a bounded local snapshot, triggered by actual owner changes.
        // A cursor wait would outlive navigation; no polling or remote fetch is used.
        EngineHost.shared.request(.appLogs(instance)) { [weak self] result in
            guard let self else { return }
            self.pending = false
            guard self.visible else { self.changed = false; return }
            os_signpost(.begin, log: Self.performance, name: "Logs response")
            defer { os_signpost(.end, log: Self.performance, name: "Logs response") }
            do {
                guard let next = try result.get()["logs"] as? LogSnapshot else { throw LogSnapshot.Failure.invalid }
                let update = self.snapshot != next || self.status != nil
                self.snapshot = next; self.status = nil
                self.message.text = next.lines.isEmpty ? "No logs yet" : nil
                self.navigationItem.rightBarButtonItem = UIBarButtonItem(title: "Copy", style: .plain, target: self, action: #selector(self.copyLogs))
                self.navigationItem.rightBarButtonItem?.isEnabled = !next.lines.isEmpty
                if update { self.tableView.reloadData() }
            } catch {
                self.changed = false
                self.showError(error.localizedDescription)
            }
            if self.changed { self.changed = false; self.refresh() }
        }
    }
    private func showError(_ text: String) {
        os_signpost(.begin, log: Self.performance, name: "Logs error")
        defer { os_signpost(.end, log: Self.performance, name: "Logs error") }
        status = String(decoding: text.utf8.prefix(4093), as: UTF8.self)
        message.text = snapshot?.lines.isEmpty != false ? status : nil
        navigationItem.rightBarButtonItem = UIBarButtonItem(title: "Retry", style: .plain, target: self, action: #selector(refresh))
        tableView.reloadData()
    }
    @objc private func copyLogs() {
        guard let snapshot, !snapshot.lines.isEmpty else { return }
        UIPasteboard.general.string = snapshot.lines.joined(separator: "\n")
        status = "Copied \(snapshot.lines.count) lines"
        tableView.reloadData()
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { snapshot?.lines.count ?? 0 }
    override func tableView(_ tableView: UITableView, titleForFooterInSection section: Int) -> String? {
        let dropped = snapshot?.dropped ?? 0
        let retained = dropped == 0 ? nil : "\(dropped) earlier log lines are no longer retained."
        return [status, retained].compactMap { $0 }.joined(separator: "\n")
    }
    override func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        os_signpost(.begin, log: Self.performance, name: "Logs cell")
        defer { os_signpost(.end, log: Self.performance, name: "Logs cell") }
        let cell = tableView.dequeueReusableCell(withIdentifier: "log-line", for: indexPath)
        var content = cell.defaultContentConfiguration()
        content.text = snapshot!.lines[indexPath.row]
        content.textProperties.numberOfLines = 0
        content.textProperties.font = UIFontMetrics(forTextStyle: .footnote).scaledFont(for: .monospacedSystemFont(ofSize: 13, weight: .regular))
        cell.contentConfiguration = content; cell.selectionStyle = .none
        return cell
    }
}
