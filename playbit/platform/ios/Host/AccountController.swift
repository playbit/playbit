import UIKit

/// Account navigation keeps recovery separate from the preserving sign-out path.
final class AccountController: UITableViewController {
    private var name: String?
    private var email: String?
    private var account: String?
    private let initial = UILabel()
    private var working = false
    private var refreshing = false
    private var copiedID = false
    private var copyTimer: Timer?
    private static let idRow = IndexPath(row: 2, section: 0)

    init(state: [String: Any]) {
        super.init(style: .insetGrouped)
        apply(state)
    }
    required init?(coder: NSCoder) { fatalError() }
    deinit { copyTimer?.invalidate(); NotificationCenter.default.removeObserver(self) }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Account"
        EnvironmentMenu.install(on: self) { [weak self] in self?.working == false }
        tableView.accessibilityIdentifier = "account-settings"
        tableView.rowHeight = UITableView.automaticDimension
        tableView.estimatedRowHeight = 52
        let header = UIView(frame: CGRect(x: 0, y: 0, width: 0, height: 248))
        initial.translatesAutoresizingMaskIntoConstraints = false
        initial.backgroundColor = .systemGray2
        initial.textColor = .white.withAlphaComponent(0.7)
        initial.font = .systemFont(ofSize: 73, weight: .semibold)
        initial.textAlignment = .center
        initial.layer.cornerRadius = 100
        initial.clipsToBounds = true
        initial.isAccessibilityElement = false
        header.addSubview(initial)
        NSLayoutConstraint.activate([
            initial.centerXAnchor.constraint(equalTo: header.centerXAnchor),
            initial.topAnchor.constraint(equalTo: header.topAnchor, constant: 16),
            initial.widthAnchor.constraint(equalToConstant: 200),
            initial.heightAnchor.constraint(equalToConstant: 200)
        ])
        tableView.tableHeaderView = header
        NotificationCenter.default.addObserver(self, selector: #selector(refresh),
            name: EngineHost.backgroundChanged, object: nil)
    }
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        refresh()
    }
    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        copyTimer?.invalidate(); copyTimer = nil
        copiedID = false
    }
    private func apply(_ state: [String: Any]) {
        name = (state["account_profile"] as? [String: Any])?["display_name"] as? String
        email = (state["email"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        account = (state["account"] as? [String: Any])?["id"] as? String
        initial.text = name?.first.map { String($0).uppercased() } ?? "?"
    }
    @objc private func refresh() {
        guard !working, !refreshing, navigationController?.topViewController === self else { return }
        refreshing = true
        EngineHost.shared.request(.status) { [weak self] result in
            guard let self else { return }
            self.refreshing = false
            switch result {
            case .success(let state):
                self.apply(state)
                self.navigationItem.prompt = nil
                self.tableView.reloadData()
            case .failure(let error): self.navigationItem.prompt = error.localizedDescription
            }
        }
    }
    override func numberOfSections(in tableView: UITableView) -> Int { 4 }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        section == 0 ? 3 : 1
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let cell = UITableViewCell(style: path.section == 2 ? .default : .value1, reuseIdentifier: nil)
        cell.textLabel?.font = .preferredFont(forTextStyle: .body)
        cell.detailTextLabel?.font = .preferredFont(forTextStyle: .body)
        cell.textLabel?.adjustsFontForContentSizeCategory = true
        cell.detailTextLabel?.adjustsFontForContentSizeCategory = true
        cell.textLabel?.numberOfLines = 0
        cell.detailTextLabel?.numberOfLines = 0
        if path.section == 0 {
            cell.textLabel?.text = ["Name", "Email address", "ID"][path.row]
            cell.detailTextLabel?.text = [name ?? "Set display name", email ?? "Unavailable", account ?? "Unavailable"][path.row]
            cell.accessibilityIdentifier = ["account-name", "account-email", "account-id"][path.row]
            if path.row == 0 {
                cell.accessoryType = .disclosureIndicator
                cell.detailTextLabel?.textColor = .label
            } else { cell.selectionStyle = .none }
            if path.row == 2 {
                var content = UIListContentConfiguration.valueCell()
                content.text = copiedID ? "✓ Copied" : "ID"
                content.secondaryText = copiedID ? nil : account.map { String($0.prefix(10)) + "…" } ?? "Unavailable"
                content.textProperties.alignment = copiedID ? .center : .natural
                content.secondaryTextProperties.numberOfLines = 1
                content.secondaryTextProperties.lineBreakMode = .byTruncatingMiddle
                content.secondaryTextProperties.font = UIFontMetrics(forTextStyle: .body)
                    .scaledFont(for: .monospacedSystemFont(ofSize: 17, weight: .regular))
                cell.contentConfiguration = content
                cell.selectionStyle = account == nil ? .none : .default
                cell.accessibilityLabel = copiedID ? "✓ Copied" : "Account ID"
                cell.accessibilityValue = copiedID ? nil : account
                cell.accessibilityHint = "Double-tap to copy the account ID"
                cell.accessibilityTraits.insert(.button)
            }
        } else if path.section == 1 {
            cell.textLabel?.text = "Private data recovery"
            cell.accessoryType = .disclosureIndicator
            cell.accessibilityIdentifier = "account-recovery"
        } else if path.section == 2 {
            var content = UIListContentConfiguration.cell()
            content.text = working ? "Signing Out…" : "Sign Out…"
            content.textProperties.color = .systemRed
            content.textProperties.alignment = .center
            cell.contentConfiguration = content
            cell.accessibilityIdentifier = "account-sign-out"
        } else {
            cell.textLabel?.text = "Copy diagnostics"
            cell.accessibilityIdentifier = "account-diagnostics"
        }
        return cell
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !working else { return }
        if path.section == 0 && path.row == 0 {
            navigationController?.pushViewController(AccountNameController(), animated: true)
        } else if path == Self.idRow {
            copyAccountID()
        } else if path.section == 1 {
            EngineHost.shared.request(.recoveryStatus) { [weak self] result in
                guard let self, self.navigationController?.topViewController === self else { return }
                switch result {
                case .success(let state):
                    self.navigationController?.pushViewController(RecoveryController(state: state), animated: true)
                case .failure(let error): self.showError(error.localizedDescription, title: "Couldn’t open recovery")
                }
            }
        } else if path.section == 2 { confirmSignOut() }
        else if path.section == 3 {
            let channel = EngineHost.shared.channel
            let build = Bundle.main.object(forInfoDictionaryKey: "PlaybitVersion") as? String ?? "Unknown"
            UIPasteboard.general.string = "Playbit \(build)\nEnvironment: \(channel?.rawValue ?? "unavailable")\nAP: \(channel?.hostname ?? "unavailable")"
            UIAccessibility.post(notification: .announcement, argument: "Diagnostics copied")
        }
    }
    private func copyAccountID() {
        guard let account else { return }
        UIPasteboard.general.string = account
        copyTimer?.invalidate()
        copiedID = true
        tableView.reloadRows(at: [Self.idRow], with: .none)
        UIAccessibility.post(notification: .announcement, argument: "Copied")
        // One explicit feedback deadline, restarted by each tap; no freshness polling.
        let timer = Timer(timeInterval: 2, repeats: false) { [weak self] _ in
            guard let self else { return }
            self.copyTimer = nil
            self.copiedID = false
            self.tableView.reloadRows(at: [Self.idRow], with: .none)
        }
        copyTimer = timer
        RunLoop.main.add(timer, forMode: .common)
    }
    private func confirmSignOut() {
        guard presentedViewController == nil else { return }
        let alert = UIAlertController(title: "Sign out?",
            message: "All local data stays on this device, including changes that haven’t synced. Signing in to a different account will ask before replacing that data. Files locations stay unavailable until you sign in again, so any edits in Files are kept.",
            preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Sign Out", style: .destructive) { [weak self] _ in self?.signOut() })
        present(alert, animated: true)
    }
    private func showError(_ message: String, title: String) {
        let alert = UIAlertController(title: title, message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
    private func signOut() {
        guard !working else { return }
        working = true
        tableView.isUserInteractionEnabled = false
        navigationItem.hidesBackButton = true
        tableView.reloadData()
        EngineHost.shared.signOut(retireApps: { GuestController.retireAccount() }) { [weak self] result in
            guard let self else { return }
            self.working = false
            self.tableView.isUserInteractionEnabled = true
            self.navigationItem.hidesBackButton = false
            switch result {
            case .success:
                self.navigationController?.setViewControllers([SessionsController(), SignInController()], animated: true)
            case .failure(let error):
                self.tableView.reloadData()
                let message = error.localizedDescription
                    + " Local data is retained. Try again, or close and reopen Playbit to resume apps."
                self.showError(message, title: "Couldn’t sign out")
            }
        }
    }
}

enum EnvironmentMenu {
    static func install(on controller: UIViewController, allowed: @escaping () -> Bool) {
        let channel = EngineHost.shared.channel
        let actions = ReleaseChannel.allCases.map { next in
            UIAction(title: next.title, state: next == channel ? .on : .off) { [weak controller] _ in
                guard let controller, let navigation = controller.navigationController else { return }
                guard next != EngineHost.shared.channel else { return }
                guard allowed(), GuestController.canSwitchEnvironment,
                      !navigation.viewControllers.contains(where: { ($0 as? DatabaseDraft)?.hasDatabaseDraft == true }) else {
                    showError("Finish the current action and save your edits before switching environments.", on: controller)
                    return
                }
                controller.view.endEditing(true)
                controller.navigationItem.rightBarButtonItem?.isEnabled = false
                EngineHost.shared.switchChannel(next, retireApps: {
                    DatabaseDocuments.cancelPendingOpen()
                    GuestController.retireAccount()
                }) { [weak controller, weak navigation] result in
                    switch result {
                    case .success:
                        navigation?.setViewControllers([SessionsController()], animated: false)
                    case .failure(let error):
                        controller?.navigationItem.rightBarButtonItem?.isEnabled = true
                        if let controller { showError(error.localizedDescription, on: controller) }
                    }
                }
            }
        }
        let item = UIBarButtonItem(title: channel?.title ?? "Environment", menu: UIMenu(title: "Environment", children: actions))
        item.accessibilityLabel = "Environment: \(channel?.title ?? "Choose environment")"
        item.accessibilityIdentifier = "environment-menu"
        controller.navigationItem.rightBarButtonItem = item
    }
    private static func showError(_ message: String, on controller: UIViewController) {
        let alert = UIAlertController(title: "Couldn’t switch environments", message: message, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        controller.present(alert, animated: true)
    }
}
