import UIKit
import os

final class SessionsController: UITableViewController {
    private static let log = OSLog(subsystem: "app.playbit", category: "sessions")
    private var invitations: [[String: Any]] = []
    private var sessions = SessionListSnapshot()
    private var examples: [CuratedExample] = []
    private var exampleError: String?
    private lazy var artwork = SessionArtwork(profile: EngineHost.shared.profile)
    private let message = UILabel()
    private let footer = UIView()
    private let startMessage = UILabel()
    private let addSession = UIButton(type: .system)
    private let accountButton = UIButton(type: .system)
    private lazy var editAction = UIBarButtonItem(image: UIImage(systemName: "pencil.circle"),
        style: .plain, target: self, action: #selector(toggleEditing))
    private var reordering = false
    private var enrolled = false
    private var runtimeError: String?
    private var refreshPending = false
    private var refreshNeeded = false
    private var automaticSignInStarted = false
    private var automaticSignInCanceled = false
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Sessions"
        artwork.changed = { [weak self] build in
            guard let self else { return }
            // Preserve native touch tracking, accessibility focus and reordering.
            for path in self.tableView.indexPathsForVisibleRows ?? [] where path.section == 1 {
                guard self.sessions.rows.indices.contains(path.row),
                      self.sessions.rows[path.row].build == build,
                      let cell = self.tableView.cellForRow(at: path) else { continue }
                self.applyArtwork(cell, row: self.sessions.rows[path.row])
            }
        }
        tableView.rowHeight = UITableView.automaticDimension
        tableView.estimatedRowHeight = 58
        tableView.separatorStyle = .none
        tableView.sectionHeaderTopPadding = 0
        tableView.layoutMargins = UIEdgeInsets(top: 0, left: 32, bottom: 0, right: 32)
        tableView.backgroundColor = .systemGroupedBackground
        tableView.accessibilityIdentifier = "sessions-list"
        tableView.allowsSelectionDuringEditing = true
        editAction.accessibilityLabel = "Edit sessions"
        accountButton.frame = CGRect(x: 0, y: 0, width: 44, height: 44)
        accountButton.titleLabel?.font = .preferredFont(forTextStyle: .headline)
        accountButton.titleLabel?.adjustsFontForContentSizeCategory = true
        accountButton.addTarget(self, action: #selector(openAccount), for: .touchUpInside)
        navigationItem.leftBarButtonItems = [UIBarButtonItem(customView: accountButton),
            UIBarButtonItem(image: UIImage(systemName: "externaldrive"), style: .plain, target: self, action: #selector(openVolumes))]
        navigationItem.leftBarButtonItems?.last?.accessibilityLabel = "Volumes"
        updateAccount(nil)
        addSession.setImage(UIImage(systemName: "plus.circle", withConfiguration:
            UIImage.SymbolConfiguration(pointSize: 38, weight: .ultraLight)), for: .normal)
        addSession.tintColor = .label
        addSession.accessibilityLabel = "New Session"
        addSession.accessibilityIdentifier = "new-session"
        addSession.addTarget(self, action: #selector(newSession), for: .touchUpInside)
        startMessage.text = "Start a session ↓"
        startMessage.font = .preferredFont(forTextStyle: .title3)
        startMessage.adjustsFontForContentSizeCategory = true
        startMessage.textColor = .secondaryLabel
        startMessage.numberOfLines = 0
        footer.addSubview(startMessage)
        footer.addSubview(addSession)
        tableView.tableFooterView = footer
        message.numberOfLines = 0
        message.textAlignment = .center
        message.textColor = .secondaryLabel
        message.text = "Opening Playbit…"
        tableView.backgroundView = message
        NotificationCenter.default.addObserver(self, selector: #selector(refresh), name: UIApplication.didBecomeActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(runtimeChanged(_:)), name: EngineHost.backgroundChanged, object: nil)
    }
    @objc private func openVolumes() {
        navigationController?.pushViewController(VolumeBrowserController(), animated: true)
    }
    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        let softwareStart = pb_ios_trace_start()
        defer { pb_ios_trace_end(PB_IOS_TRACE_SESSIONS_LAYOUT, softwareStart) }
        os_signpost(.begin, log: Self.log, name: "Sessions bottom layout")
        defer { os_signpost(.end, log: Self.log, name: "Sessions bottom layout") }
        let width = tableView.bounds.width
        let inset = tableView.layoutMargins.left
        let empty = sessions.rows.isEmpty && invitations.isEmpty
        startMessage.isHidden = !empty || isEditing
        addSession.isHidden = isEditing
        let labelHeight = empty ? startMessage.sizeThatFits(CGSize(width: max(0, width - inset * 2),
            height: .greatestFiniteMagnitude)).height : 0
        startMessage.frame = CGRect(x: inset + 6, y: 8, width: max(0, width - inset * 2), height: labelHeight)
        addSession.frame = CGRect(x: inset, y: empty ? labelHeight + 28 : 4, width: 52, height: 52)
        let height: CGFloat = isEditing ? 0 : addSession.frame.maxY + 16
        if footer.frame.width != width || footer.frame.height != height {
            footer.frame = CGRect(x: 0, y: 0, width: width, height: height)
            tableView.tableFooterView = footer
        }
        // Insets keep short lists against the bottom safe area without making
        // hundreds of offscreen cells or changing native scrolling behavior.
        let top = max(0, tableView.bounds.height - tableView.safeAreaInsets.top
            - tableView.safeAreaInsets.bottom - tableView.contentSize.height)
        if abs(tableView.contentInset.top - top) > 0.5 {
            let wasAtTop = tableView.contentOffset.y <= -tableView.adjustedContentInset.top + 1
            tableView.contentInset.top = top
            if wasAtTop && !tableView.isDragging && !tableView.isDecelerating {
                tableView.contentOffset.y = -tableView.adjustedContentInset.top
            }
        }
    }
    @objc private func toggleEditing() {
        guard !reordering else { return }
        setEditing(!isEditing, animated: true)
    }
    override func setEditing(_ editing: Bool, animated: Bool) {
        super.setEditing(editing, animated: animated)
        title = editing ? "Editing Sessions" : "Sessions"
        navigationItem.rightBarButtonItem?.image = UIImage(systemName: editing ? "checkmark.circle.fill" : "pencil.circle")
        navigationItem.rightBarButtonItem?.accessibilityLabel = editing ? "Done" : "Edit sessions"
        if !editing && sessions.rows.isEmpty { navigationItem.rightBarButtonItem = nil }
        tableView.reloadData()
        view.setNeedsLayout()
    }
    private func updateAccount(_ value: [String: Any]?) {
        let profile = value?["account_profile"] as? [String: Any]
        let name = (profile?["display_name"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines)
        if enrolled, let initial = name?.first {
            accountButton.setTitle(String(initial).uppercased(), for: .normal)
            accountButton.setImage(nil, for: .normal)
        } else {
            accountButton.setTitle(nil, for: .normal)
            accountButton.setImage(UIImage(systemName: "person.crop.circle"), for: .normal)
        }
        accountButton.accessibilityLabel = enrolled ? "Account" : "Sign in"
        accountButton.accessibilityValue = enrolled ? name : nil
    }
    @objc private func openAccount() {
        guard enrolled else { signIn(); return }
        EngineHost.shared.request(.status) { result in
            guard self.navigationController?.topViewController === self else { return }
            switch result {
            case .success(let state):
                self.navigationController?.pushViewController(AccountController(state: state), animated: true)
            case .failure(let error): self.showError(error, title: "Couldn’t open account")
            }
        }
    }
    deinit { NotificationCenter.default.removeObserver(self) }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); refresh() }
    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        automaticSignInCanceled = true
    }
    @objc private func runtimeChanged(_ notification: Notification) {
        if let error = notification.userInfo?["error"] as? String { runtimeError = error; message.text = error }
        refresh()
    }
    @objc private func refresh() {
        // Load local state during foreground activation so a restored sign-in
        // decision is ready without waiting for the launch transition to finish.
        guard UIApplication.shared.applicationState != .background,
            navigationController?.topViewController === self else { return }
        guard !reordering else { refreshNeeded = true; return }
        guard !refreshPending else { refreshNeeded = true; return }
        refreshPending = true
        EngineHost.shared.request(.status) { result in
            self.refreshPending = false
            // A status reply queued before the drag must not roll back its
            // immediate local ordering while the durable request is pending.
            if self.reordering { self.refreshNeeded = true; return }
            defer {
                if self.refreshNeeded {
                    self.refreshNeeded = false
                    self.refresh()
                }
            }
            switch result {
            case .success(let value):
                let snapshot: SessionListSnapshot
                let timing = Self.log
                let signpost = OSSignpostID(log: timing)
                let softwareStart = pb_ios_trace_start()
                os_signpost(.begin, log: timing, name: "Session row labels", signpostID: signpost)
                do { snapshot = try SessionListSnapshot(value) }
                catch {
                    os_signpost(.end, log: timing, name: "Session row labels", signpostID: signpost)
                    pb_ios_trace_end(PB_IOS_TRACE_SESSIONS_LABELS, softwareStart)
                    self.message.text = error.localizedDescription
                    self.navigationItem.prompt = "Couldn’t refresh sessions"
                    if self.navigationController?.topViewController === self && self.presentedViewController == nil {
                        self.showError(error, title: "Couldn’t refresh sessions")
                    }
                    return
                }
                os_signpost(.end, log: timing, name: "Session row labels", signpostID: signpost)
                pb_ios_trace_end(PB_IOS_TRACE_SESSIONS_LABELS, softwareStart)
                let invitations = value["invitations"] as? [[String: Any]] ?? []
                // Runtime notifications include replication progress that does
                // not change these rows. Reloading identical cells interrupts
                // native touch tracking and accessibility focus.
                let rowsChanged = self.sessions.rows != snapshot.rows
                    || !NSArray(array: self.invitations).isEqual(to: invitations)
                self.invitations = invitations
                self.sessions = snapshot
                do {
                    self.examples = try CuratedExample.parse(value["examples"])
                    self.exampleError = nil
                } catch {
                    self.examples = []
                    self.exampleError = error.localizedDescription
                }
                self.artwork.reconcile(snapshot.rows)
                self.enrolled = value["enrolled"] as? Bool == true
                self.navigationItem.prompt = nil
                self.updateAccount(value)
                self.message.text = self.runtimeError
                if rowsChanged { self.tableView.reloadData() }
                self.navigationItem.rightBarButtonItem = self.isEditing || !snapshot.rows.isEmpty ? self.editAction : nil
                self.editAction.isEnabled = !self.reordering
                self.view.setNeedsLayout()
                if (value["account_profile"] as? [String: Any])?["replacement_required"] as? Bool == true {
                    self.automaticSignInStarted = true
                    let controller = SignInController()
                    controller.replacementRequired = true
                    self.navigationController?.pushViewController(controller, animated: false)
                    return
                }
                if self.enrolled, let profile = value["account_profile"] as? [String: Any],
                    profile["agreement_required"] as? Bool == true,
                    let account = (value["account"] as? [String: String])?["id"],
                    self.navigationController?.topViewController === self, self.presentedViewController == nil {
                    self.navigationController?.pushViewController(UserAgreementController(account: account), animated: false)
                    return
                }
                if !self.automaticSignInStarted && UIApplication.shared.applicationState == .active {
                    self.automaticSignInStarted = true
                    if !self.enrolled && self.invitations.isEmpty && !self.automaticSignInCanceled {
                        EngineHost.shared.request(.prepareSignIn) { result in
                            let present = !self.automaticSignInCanceled && !self.enrolled
                                && self.invitations.isEmpty && self.presentedViewController == nil
                                && UIApplication.shared.applicationState == .active
                                && self.navigationController?.topViewController === self
                            self.automaticSignInCanceled = true
                            if present, case .success(let value) = result,
                                value["sign_in_available"] as? Bool == true {
                                let controller = SignInController()

                                self.navigationController?.pushViewController(controller, animated: true)
                            }
                        }
                    }
                }
            case .failure(let error):
                if (error as NSError).domain == "Playbit", (error as NSError).code == 2 {
                    self.navigationController?.setViewControllers([CorruptStoreController()], animated: false)
                } else {
                    self.message.text = error.localizedDescription
                }
            }
        }
    }
    func receiveInvitation(_ invitation: String) {
        automaticSignInCanceled = true
        EngineHost.shared.request(.previewInvitation(invitation)) { result in
            switch result {
            case .success(let preview):
                self.presentInvitation(preview)
            case .failure(let error): self.showError(error)
            }
        }
    }
    @objc private func newSession() {
        automaticSignInCanceled = true
        let entry = NewSessionController(enrolled: enrolled, examples: examples, exampleError: exampleError,
            onPreview: { [weak self] preview in self?.presentInvitation(preview) },
            onSession: { [weak self] session in
                self?.dismiss(animated: true) { [weak self] in
                    self?.navigationController?.pushViewController(GuestController.chosen(session: session), animated: true)
                }
            },
            onSignIn: { [weak self] in self?.dismiss(animated: true) { [weak self] in self?.signIn() } })
        let sheet = EnvironmentNavigationController(rootViewController: entry)
        sheet.modalPresentationStyle = .pageSheet
        sheet.sheetPresentationController?.detents = [.large()]
        sheet.sheetPresentationController?.prefersGrabberVisible = true
        present(sheet, animated: true)
    }
    private func presentInvitation(_ preview: [String: Any]) {
        let show = { [weak self] in
            guard let self else { return }
            self.navigationController?.popToRootViewController(animated: false)
            // A link to a session this device already holds opens it directly.
            if preview["retained"] as? Bool == true, let session = preview["session"] as? String {
                self.navigationController?.pushViewController(GuestController.chosen(session: session), animated: true)
            } else {
                self.navigationController?.pushViewController(InvitationController(preview: preview), animated: true)
            }
        }
        if presentedViewController != nil { dismiss(animated: true, completion: show) }
        else { show() }
    }
    @objc private func signIn() {
        automaticSignInCanceled = true
        navigationController?.pushViewController(SignInController(), animated: true)
    }
    override func numberOfSections(in tableView: UITableView) -> Int { 2 }
    override func tableView(_ tableView: UITableView, titleForHeaderInSection section: Int) -> String? {
        section == 0 && !invitations.isEmpty ? "Invitations" : nil
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        section == 0 ? invitations.count : sessions.rows.count
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let cell = tableView.dequeueReusableCell(withIdentifier: "host-row")
            ?? UITableViewCell(style: .subtitle, reuseIdentifier: "host-row")
        cell.backgroundColor = .clear
        cell.textLabel?.font = path.section == 1
            ? UIFontMetrics(forTextStyle: .title3).scaledFont(for: .systemFont(ofSize: 20, weight: .semibold))
            : .preferredFont(forTextStyle: .body)
        cell.detailTextLabel?.font = .preferredFont(forTextStyle: .subheadline)
        cell.textLabel?.textColor = .label
        cell.detailTextLabel?.textColor = .secondaryLabel
        cell.textLabel?.adjustsFontForContentSizeCategory = true
        cell.detailTextLabel?.adjustsFontForContentSizeCategory = true
        cell.textLabel?.numberOfLines = 0
        cell.detailTextLabel?.numberOfLines = 0
        cell.contentConfiguration = nil
        cell.imageView?.image = nil
        cell.accessibilityValue = nil
        cell.accessibilityLabel = nil
        cell.accessoryType = .none
        cell.editingAccessoryType = .none
        if path.section == 0 {
            let preview = invitations[path.row]
            cell.textLabel?.text = InvitationController.rootName(preview) ?? "Invitation"
            cell.detailTextLabel?.text = preview["paused"] as? Bool == true ? "Paused · Tap to resume" : "Tap to join"
            cell.accessibilityIdentifier = "invitation-row"
            cell.accessoryType = .disclosureIndicator
        } else if path.section == 1 {
            let row = sessions.rows[path.row]
            cell.textLabel?.text = row.title
            cell.detailTextLabel?.text = row.subtitle ?? (isEditing ? String(row.id.prefix(8)) : nil)
            cell.editingAccessoryType = .disclosureIndicator
            cell.accessibilityIdentifier = "session-row:" + row.id
            cell.accessibilityLabel = row.title
            cell.accessibilityValue = row.subtitle
            applyArtwork(cell, row: row)
        }
        return cell
    }
    private func applyArtwork(_ cell: UITableViewCell, row: SessionListSnapshot.Row) {
        var content: UIListContentConfiguration
        if let existing = cell.contentConfiguration as? UIListContentConfiguration {
            content = existing
        } else {
            content = .subtitleCell()
            content.textProperties.font = UIFontMetrics(forTextStyle: .title3)
                .scaledFont(for: .systemFont(ofSize: 20, weight: .semibold))
            content.textProperties.numberOfLines = 0
            content.secondaryTextProperties.font = .preferredFont(forTextStyle: .subheadline)
            content.secondaryTextProperties.color = .secondaryLabel
            content.secondaryTextProperties.numberOfLines = 0
            content.imageProperties.maximumSize = CGSize(width: 40, height: 40)
            content.imageProperties.reservedLayoutSize = CGSize(width: 40, height: 40)
            content.imageToTextPadding = 16
            content.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 8, leading: 40, bottom: 8, trailing: 32)
        }
        content.text = row.title
        let subtitle = row.subtitle ?? (isEditing ? String(row.id.prefix(8)) : nil)
        content.secondaryText = artwork.failed(row.build)
            ? [subtitle, "App artwork unavailable"].compactMap { $0 }.joined(separator: " · ") : subtitle
        content.image = artwork.image(row.build)
        cell.contentConfiguration = content
        cell.accessibilityValue = artwork.failed(row.build)
            ? [row.subtitle, "App artwork unavailable"].compactMap { $0 }.joined(separator: ". ")
            : row.subtitle
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        if path.section == 0 {
            navigationController?.pushViewController(InvitationController(preview: invitations[path.row]), animated: true)
        } else if path.section == 1 {
            let id = sessions.rows[path.row].id
            let controller: UIViewController = isEditing ? SessionDetailsController(session: id) : GuestController.chosen(session: id)
            navigationController?.pushViewController(controller, animated: true)
        }
    }
    override func tableView(_ tableView: UITableView, editingStyleForRowAt path: IndexPath) -> UITableViewCell.EditingStyle { .none }
    override func tableView(_ tableView: UITableView, shouldIndentWhileEditingRowAt path: IndexPath) -> Bool { false }
    override func tableView(_ tableView: UITableView, canMoveRowAt path: IndexPath) -> Bool { path.section == 1 && !reordering }
    override func tableView(_ tableView: UITableView, targetIndexPathForMoveFromRowAt source: IndexPath, toProposedIndexPath proposed: IndexPath) -> IndexPath {
        proposed.section == 1 ? proposed : source
    }
    override func tableView(_ tableView: UITableView, moveRowAt source: IndexPath, to destination: IndexPath) {
        sessions.move(from: source.row, to: destination.row)
        reordering = true
        navigationItem.rightBarButtonItem?.isEnabled = false
        EngineHost.shared.request(.orderSessions(sessions.rows.map(\.id))) { [weak self] result in
            guard let self else { return }
            self.reordering = false
            self.navigationItem.rightBarButtonItem?.isEnabled = true
            if case .failure(let error) = result { self.showError(error, title: "Couldn’t reorder sessions") }
            self.refresh()
        }
    }
    private func showError(_ error: Error, title: String = "Couldn’t open invitation") {
        let alert = UIAlertController(title: title, message: error.localizedDescription, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
}

/// Session details use retained identity and session mounts. Leave is local.
final class SessionDetailsController: UITableViewController {
    private let session: String
    private var reference: String?
    private var appName = "Loading…"
    private var rootName = "Loading…"
    private var version: String?
    private var grants: [PersonalVolumeAccess.Row] = []
    private var pending = false
    private var refreshing = false
    private var refreshNeeded = false
    private var failure: String?
    init(session: String) { self.session = session; super.init(style: .insetGrouped) }
    required init?(coder: NSCoder) { fatalError() }
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Editing Session"
        tableView.rowHeight = UITableView.automaticDimension
        tableView.estimatedRowHeight = 52
        NotificationCenter.default.addObserver(self, selector: #selector(refresh), name: EngineHost.backgroundChanged, object: nil)
    }
    deinit { NotificationCenter.default.removeObserver(self) }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); refresh() }
    @objc private func refresh() {
        guard navigationController?.topViewController === self else { return }
        guard !pending && !refreshing else { refreshNeeded = true; return }
        refreshing = true
        EngineHost.shared.request(.status) { [weak self] result in
            guard let self else { return }
            do {
                let value = try result.get()
                let snapshot = try SessionListSnapshot(value)
                guard let row = snapshot.rows.first(where: { $0.id == self.session }),
                    let records = value["sessions"] as? [[String: Any]],
                    let record = records.first(where: { $0["id"] as? String == self.session }),
                    let build = record["resolved_build_id"] as? String, Identifier.valid(build, bytes: 32),
                    let root = record["root_volume"] as? String else { throw SessionListSnapshot.Failure.invalid }
                let volumes = value["volumes"] as? [[String: Any]] ?? []
                let rootName = volumes.first(where: { $0["id"] as? String == root })?["name"] as? String ?? root
                let changed = self.appName != row.title || self.rootName != rootName || self.version != build || self.reference != row.reference
                self.reference = row.reference
                self.appName = row.title
                self.rootName = rootName
                self.version = build
                EngineHost.shared.request(.personalVolumes(self.session)) { [weak self] result in
                    guard let self else { return }
                    do {
                        let grants = try PersonalVolumeAccess(result.get()).rows.filter { $0.choice != 0 }
                        let changed = changed || self.grants != grants
                        self.grants = grants
                        self.finishRefresh(nil, changed: changed)
                    } catch { self.finishRefresh(error) }
                }
            } catch { self.finishRefresh(error) }
        }
    }
    private func finishRefresh(_ error: Error?, changed: Bool = true) {
        refreshing = false
        let failure = error?.localizedDescription
        let changed = changed || self.failure != failure
        self.failure = failure
        if pending { refreshNeeded = true; return }
        if changed { tableView.reloadData() }
        if refreshNeeded { refreshNeeded = false; refresh() }
    }
    private func finishAction(_ error: Error) {
        pending = false
        failure = error.localizedDescription
        tableView.reloadData()
        if refreshNeeded { refreshNeeded = false; refresh() }
    }
    override func numberOfSections(in tableView: UITableView) -> Int { 2 }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        section == 0 ? 5 : max(1, grants.count)
    }
    override func tableView(_ tableView: UITableView, titleForHeaderInSection section: Int) -> String? {
        section == 1 ? "Attached Volumes" : nil
    }
    override func tableView(_ tableView: UITableView, titleForFooterInSection section: Int) -> String? {
        section == 0 ? failure : "Personal volume access applies to this build. Tap to review or change access."
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let action = path.section == 0 && path.row >= 3
        let reuse = action ? "session-detail-action" : "session-detail"
        let cell = tableView.dequeueReusableCell(withIdentifier: reuse)
            ?? UITableViewCell(style: action ? .default : .value1, reuseIdentifier: reuse)
        cell.textLabel?.font = .preferredFont(forTextStyle: .body)
        cell.detailTextLabel?.font = .preferredFont(forTextStyle: .body)
        cell.textLabel?.adjustsFontForContentSizeCategory = true
        cell.detailTextLabel?.adjustsFontForContentSizeCategory = true
        cell.textLabel?.numberOfLines = 0
        cell.detailTextLabel?.numberOfLines = 0
        cell.textLabel?.textAlignment = .natural
        cell.textLabel?.textColor = .label
        cell.detailTextLabel?.text = nil
        cell.accessibilityValue = nil
        cell.accessibilityTraits = []
        cell.accessoryType = .none
        cell.selectionStyle = .none
        cell.isUserInteractionEnabled = false
        cell.accessibilityIdentifier = "session-detail-\(path.section)-\(path.row)"
        if path.section == 1 {
            cell.textLabel?.text = grants.isEmpty ? "Choose personal volumes…" : grants[path.row].name
            cell.detailTextLabel?.text = grants.isEmpty ? nil : (grants[path.row].choice == 1 ? "Read only" : "Read and write")
            cell.accessoryType = .disclosureIndicator
            cell.selectionStyle = .default
            cell.isUserInteractionEnabled = version != nil && !pending
        } else if path.row < 3 {
            cell.textLabel?.text = ["Session ID", "Application", "Root volume"][path.row]
            cell.detailTextLabel?.text = [reference ?? "Unavailable", appName, rootName][path.row]
            if path.row == 0 { cell.accessibilityValue = reference }
        } else {
            cell.textLabel?.text = path.row == 3 ? "Live Share" : "Leave Session"
            cell.textLabel?.textAlignment = .center
            cell.textLabel?.textColor = pending ? .tertiaryLabel : (path.row == 3 ? .systemBlue : .systemRed)
            cell.selectionStyle = .default
            cell.isUserInteractionEnabled = !pending && version != nil
            cell.accessibilityTraits.insert(.button)
            if !cell.isUserInteractionEnabled { cell.accessibilityTraits.insert(.notEnabled) }
        }
        return cell
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !pending else { return }
        if path.section == 1, version != nil {
            navigationController?.pushViewController(PersonalVolumesController(session: session), animated: true)
        } else if path.row == 3 {
            pending = true; tableView.reloadData()
            EngineHost.shared.request(.invitation(session)) { [weak self] result in
                guard let self else { return }
                self.pending = false; self.tableView.reloadData()
                guard self.navigationController?.topViewController === self else { return }
                do {
                    let value = try result.get()
                    guard let text = value["invitation"] as? String, let url = URL(string: text) else { throw SessionListSnapshot.Failure.invalid }
                    let sheet = UIActivityViewController(activityItems: [url], applicationActivities: nil)
                    sheet.popoverPresentationController?.sourceView = self.tableView.cellForRow(at: path)
                    self.present(sheet, animated: true)
                } catch { self.finishAction(error) }
            }
        } else if path.row == 4 {
            let alert = UIAlertController(title: "Leave this session?",
                message: "Remove it from this device’s list. Shared data and other participants are unchanged. You can rejoin using its invitation.", preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
            alert.addAction(UIAlertAction(title: "Leave Session", style: .destructive) { [weak self] _ in self?.leave() })
            present(alert, animated: true)
        }
    }
    private func leave() {
        guard !pending else { return }
        pending = true; tableView.reloadData()
        EngineHost.shared.request(.forgetSession(session)) { [weak self] result in
            guard let self else { return }
            switch result {
            case .success:
                self.pending = false
                self.navigationController?.popViewController(animated: true)
            case .failure(let error): self.finishAction(error)
            }
        }
    }
}

final class InvitationController: UIViewController {
    private var preview: [String: Any]
    private let heading = UILabel()
    private lazy var pauseAction = UIBarButtonItem(title: "Pause", style: .plain, target: self, action: #selector(pause))
    private let message = UILabel()
    private let action = UIButton(type: .system)
    private let progress = UIActivityIndicatorView(style: .medium)
    private var refreshNeeded = false
    private var busy = false
    private var joining = false
    private var joinRequested = false
    private var paused = false
    var recoveryPermitted = false
    private var agreementAccount: String?
    private var state = "checking"
    private var session: String { preview["session"] as? String ?? "" }
    /// Present once the creator's registration has been read.
    static func rootName(_ preview: [String: Any]) -> String? {
        (preview["received"] as? [String: Any])?["root_name"] as? String
    }
    init(preview: [String: Any]) { self.preview = preview; self.paused = preview["paused"] as? Bool ?? false; super.init(nibName: nil, bundle: nil) }
    required init?(coder: NSCoder) { fatalError() }
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Invitation"
        view.backgroundColor = .systemBackground
        navigationItem.rightBarButtonItem = paused ? nil : pauseAction
        heading.text = Self.rootName(preview) ?? "Join a shared app"
        heading.font = .preferredFont(forTextStyle: .largeTitle)
        heading.numberOfLines = 0
        message.numberOfLines = 0
        message.accessibilityIdentifier = "invitation-status"
        let privacy = UILabel()
        privacy.text = "This invitation connects the app’s shared data. Your personal volumes remain unavailable unless you choose them."
        privacy.font = .preferredFont(forTextStyle: .footnote)
        privacy.textColor = .secondaryLabel
        privacy.numberOfLines = 0
        action.configuration = .filled()
        action.accessibilityIdentifier = "invitation-action"
        action.addTarget(self, action: #selector(activate), for: .touchUpInside)
        let stack = UIStackView(arrangedSubviews: [heading, message, progress, action, privacy])
        stack.axis = .vertical
        stack.spacing = 24
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.layoutMarginsGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: view.layoutMarginsGuide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 32)
        ])
        NotificationCenter.default.addObserver(self, selector: #selector(foreground), name: UIApplication.didBecomeActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(runtimeChanged), name: EngineHost.backgroundChanged, object: nil)
    }
    deinit { NotificationCenter.default.removeObserver(self) }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); foreground() }

    @objc private func foreground() {
        guard isViewLoaded, navigationController?.topViewController === self else { return }
        if paused { show(["kind": "paused"]); return }
        if joining { checkTransfer() } else { checkAccount { self.check() } }
    }
    private func showRequiredAgreement(_ status: [String: Any]) -> Bool {
        guard status["enrolled"] as? Bool == true,
            (status["account_profile"] as? [String: Any])?["agreement_required"] as? Bool == true else { return false }
        guard let account = (status["account"] as? [String: String])?["id"], Identifier.valid(account) else {
            joinFailed("Couldn’t identify your signed-in account.")
            return true
        }
        agreementAccount = account
        state = "agreement"
        joinRequested = false
        progress.stopAnimating()
        message.text = "Review the Playbit User Agreement before joining this shared app."
        configureAction("Review agreement", enabled: true)
        return true
    }
    private func checkAccount(beforeJoin: Bool = false, then next: @escaping () -> Void) {
        guard !busy, !paused else { return }
        busy = true
        EngineHost.shared.request(.status) { result in
            self.busy = false
            defer { self.drainRefresh() }
            guard !self.paused, self.navigationController?.topViewController === self else { return }
            switch result {
            case .success(let status):
                if beforeJoin && status["enrolled"] as? Bool != true {
                    self.show(["kind": "sign_in"])
                } else if !self.showRequiredAgreement(status) {
                    next()
                }
            case .failure(let error): self.joinFailed(error.localizedDescription)
            }
        }
    }
    @objc private func runtimeChanged() {
        guard isViewLoaded, navigationController?.topViewController === self,
            UIApplication.shared.applicationState == .active, !paused else { return }
        guard !busy else { refreshNeeded = true; return }
        if joining { checkTransfer(); return }
        busy = true
        EngineHost.shared.request(.status) { result in
            self.busy = false
            defer { self.drainRefresh() }
            guard !self.paused else { return }
            switch result {
            case .success(let value):
                if self.showRequiredAgreement(value) { return }
                if let pending = (value["invitations"] as? [[String: Any]])?.first(where: { $0["session"] as? String == self.session }),
                    let name = Self.rootName(pending) {
                    self.preview = pending
                    self.heading.text = name
                }
                if let status = value["invitation_status"] as? [String: Any],
                    status["session"] as? String == self.session,
                    let state = status["state"] as? [String: Any] { self.show(state) }
            case .failure(let error): self.message.text = error.localizedDescription
            }
        }
    }
    private func drainRefresh() {
        if refreshNeeded { refreshNeeded = false; runtimeChanged() }
    }
    private func check(resume: Bool = false) {
        guard !busy, !joining, (!paused || resume), UIApplication.shared.applicationState == .active else { return }
        busy = true
        EngineHost.shared.request(resume ? .resumeInvitation(session) : .watchInvitation(session)) { result in
            self.busy = false
            defer { self.drainRefresh() }
            if resume, case .success = result { self.paused = false }
            guard !self.paused || resume else { return }
            let joinRequested = self.joinRequested
            self.joinRequested = false
            switch result {
            case .success(let value):
                let status = value["invitation_status"] as? [String: Any] ?? [:]
                self.show(status["state"] as? [String: Any] ?? [:])
                if joinRequested && (self.state == "ready" || self.state == "waiting_peer") { self.activate() }
            case .failure(let error):
                self.progress.stopAnimating()
                self.state = "retry"
                self.message.text = "Couldn’t check this invitation. \(error.localizedDescription)\nYour invitation is saved. Try again when you’re connected."
                self.configureAction("Try again", enabled: true)
            }
        }
    }
    private func show(_ value: [String: Any]) {
        state = value["kind"] as? String ?? "retry"
        navigationItem.rightBarButtonItem = state == "paused" ? nil : pauseAction
        progress.stopAnimating()
        switch state {
        case "checking":
            message.text = "Checking invitation…"
            progress.startAnimating()
            configureAction("Checking…", enabled: false)
        case "paused":
            message.text = "Invitation paused. Resume whenever you’re ready."
            configureAction("Resume", enabled: true)
        case "sign_in":
            message.text = "Sign in to join. Your invitation will stay here while you do."
            configureAction("Sign in", enabled: true)
        case "ready":
            message.text = "The app is ready. Join to download it and open your shared session."
            configureAction("Join", enabled: true)
        case "waiting_registration":
            message.text = "The sender is still preparing this invitation. This page will update when it’s ready."
            configureAction("Waiting for invitation…", enabled: false)
        case "waiting_app":
            message.text = "The app is still uploading. Your invitation is saved; you can leave and return."
            configureAction("Waiting for app…", enabled: false)
        case "waiting_database":
            message.text = "The shared data is still being prepared. This page will update when it’s ready."
            configureAction("Waiting for data…", enabled: false)
        case "waiting_peer":
            message.text = "This app’s private data needs an authorized device online. Join to try connecting to one, or return later."
            configureAction("Join", enabled: true)
        case "denied":
            message.text = "\(value["message"] as? String ?? "Access is no longer available.")\nAsk the sender for an invitation with access."
            configureAction("Check again", enabled: true)
        case "failed":
            message.text = "\(value["message"] as? String ?? "Couldn’t check this invitation.")\nYour invitation is saved. Try again when you’re connected."
            configureAction("Try again", enabled: true)
        default:
            message.text = "The invitation could not be opened. Try checking again."
            configureAction("Try again", enabled: true)
        }
    }
    private func configureAction(_ title: String, enabled: Bool) {
        action.setTitle(title, for: .normal)
        action.isEnabled = enabled
    }
    @objc private func activate() {
        if paused { check(resume: true); return }
        if state == "agreement", let account = agreementAccount {
            navigationController?.pushViewController(UserAgreementController(account: account), animated: true)
            return
        }
        if state == "sign_in" {
            navigationController?.pushViewController(SignInController(), animated: true)
            return
        }
        guard state == "ready" || state == "waiting_peer" else { check(); return }
        if busy {
            // An in-flight state read must not discard an intentional Join tap. Its
            // response still decides whether access is ready before we proceed.
            joinRequested = true
            message.text = "Checking access before joining…"
            progress.startAnimating()
            configureAction("Joining…", enabled: false)
            return
        }
        if !recoveryPermitted {
            busy = true
            message.text = "Checking private access before joining…"
            progress.startAnimating()
            configureAction("Joining…", enabled: false)
            EngineHost.shared.request(.recoveryStatus) { result in
                self.busy = false
                guard !self.paused, self.navigationController?.topViewController === self else { return }
                self.progress.stopAnimating()
                self.configureAction("Join", enabled: true)
                switch result {
                case .failure(let error): self.message.text = error.localizedDescription
                case .success(let value):
                    if value["kind"] as? String == "ready" { self.recoveryPermitted = true; self.activate() }
                    else {
                        let recovery = RecoveryController(state: value, allowInvitation: true) { [weak self] in self?.recoveryPermitted = true }
                        self.navigationController?.pushViewController(recovery, animated: true)
                    }
                }
            }
            return
        }
        progress.startAnimating()
        configureAction("Joining…", enabled: false)
        checkAccount(beforeJoin: true) { self.startJoin() }
    }
    private func startJoin() {
        busy = true
        joining = true
        message.text = "Downloading the app and preparing shared data…"
        progress.startAnimating()
        configureAction("Joining…", enabled: false)
        EngineHost.shared.request(.startJoin(session)) { result in
            self.busy = false
            defer { self.drainRefresh() }
            guard !self.paused else { return }
            switch result {
            case .success: self.joining = true; self.foreground()
            case .failure(let error): self.joinFailed(error.localizedDescription)
            }
        }
    }
    private func joinFailed(_ text: String) {
        joining = false
        progress.stopAnimating()
        message.text = "Couldn’t join yet. \(text)\nYour invitation is saved."
        configureAction("Try again", enabled: true)
        refreshNeeded = false
    }
    private func checkTransfer() {
        guard joining, !busy, !paused, UIApplication.shared.applicationState == .active else { return }
        busy = true
        EngineHost.shared.request(.transferStatus) { result in
            self.busy = false
            defer { self.drainRefresh() }
            guard !self.paused else { return }
            switch result {
            case .failure(let error): self.joinFailed(error.localizedDescription)
            case .success(let value):
                let transfer = value["transfer"] as? [String: Any] ?? [:]
                switch transfer["kind"] as? String {
                case "complete":
                    guard transfer["session"] as? String == self.session else { self.joining = false; self.activate(); return }
                    self.joining = false; self.progress.stopAnimating(); self.refreshNeeded = false
                    let guest = GuestController.chosen(session: self.session)
                    var controllers = self.navigationController?.viewControllers ?? []
                    controllers.removeAll { $0 === self }; controllers.append(guest)
                    self.navigationController?.setViewControllers(controllers, animated: true)
                case "failed": self.joinFailed(transfer["message"] as? String ?? "The transfer failed. Try again.")
                case "paused", "canceling":
                    self.joining = false; self.paused = true
                    self.show(["kind": "paused"])
                default: self.message.text = transfer["message"] as? String ?? "Downloading the app and preparing shared data…"
                }
            }
        }
    }
    @objc private func pause() {
        joinRequested = false
        EngineHost.shared.request(.pauseInvitation(session)) { result in
            switch result {
            case .success:
                self.paused = true
                self.refreshNeeded = false
                self.navigationController?.popViewController(animated: true)
            case .failure(let error): self.message.text = error.localizedDescription
            }
        }
    }
}

final class SignInController: UIViewController, UITextFieldDelegate {
    var replacementRequired = false
    private var replacementPresented = false
    private var replacementStatusPending = false
    private let email = UITextField()
    private let code = UITextField()
    private let accountKey = UITextField()
    private let message = UILabel()
    private let action = UIButton(type: .system)
    private var prefillAllowed = true
    private let resend = UIButton(type: .system)
    private let changeEmail = UIButton(type: .system)
    private var busy = false
    private var sendingCode = false
    private var sent = false
    private var authenticated = false
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Sign in"
        EnvironmentMenu.install(on: self) { [weak self] in self?.busy == false }
        view.backgroundColor = .systemBackground
        message.text = EngineHost.shared.channel == nil ? "Choose an environment to continue." : "Enter your email address to continue."
        message.numberOfLines = 0
        NotificationCenter.default.addObserver(self, selector: #selector(checkReplacement), name: EngineHost.backgroundChanged, object: nil)
        message.accessibilityIdentifier = "sign-in-status"
        email.placeholder = "Email address"
        email.keyboardType = .emailAddress
        email.textContentType = .emailAddress
        email.autocapitalizationType = .none
        email.autocorrectionType = .no
        email.borderStyle = .roundedRect
        email.returnKeyType = .send
        email.delegate = self
        email.addTarget(self, action: #selector(edited), for: .editingChanged)
        email.accessibilityIdentifier = "sign-in-email"
        accountKey.placeholder = "Account key (optional)"
        accountKey.isSecureTextEntry = true
        accountKey.textContentType = .password
        accountKey.autocapitalizationType = .none
        accountKey.autocorrectionType = .no
        accountKey.borderStyle = .roundedRect
        accountKey.returnKeyType = .go
        accountKey.delegate = self
        accountKey.accessibilityIdentifier = "sign-in-account-key"
        accountKey.addTarget(self, action: #selector(edited), for: .editingChanged)
        code.placeholder = "6-digit code"
        code.keyboardType = .numberPad
        code.textContentType = .oneTimeCode
        code.borderStyle = .roundedRect
        code.addTarget(self, action: #selector(updateControls), for: .editingChanged)
        code.isHidden = true
        code.accessibilityIdentifier = "sign-in-code"
        action.configuration = .filled()
        action.setTitle("Continue", for: .normal)
        action.accessibilityIdentifier = "sign-in-submit"
        action.addTarget(self, action: #selector(submit), for: .touchUpInside)
        resend.setTitle("Send code again", for: .normal)
        resend.accessibilityIdentifier = "sign-in-resend"
        resend.isHidden = true
        resend.addTarget(self, action: #selector(requestCode), for: .touchUpInside)
        changeEmail.setTitle("Change email", for: .normal)
        changeEmail.accessibilityIdentifier = "sign-in-change-email"
        changeEmail.addTarget(self, action: #selector(editEmail), for: .touchUpInside)
        let explanation = UILabel()
        explanation.text = "We use your email for sign-in. Leave the account key empty to receive a code. Our privacy policy explains how we handle account and connection information."
        explanation.numberOfLines = 0
        explanation.font = .preferredFont(forTextStyle: .footnote)
        explanation.textColor = .secondaryLabel
        let terms = UIButton(type: .system)
        terms.setTitle("User agreement", for: .normal)
        terms.addAction(UIAction { [weak self] _ in
            self?.navigationController?.pushViewController(UserAgreementController(), animated: true)
        }, for: .touchUpInside)
        let privacy = UIButton(type: .system)
        privacy.setTitle("Privacy policy", for: .normal)
        privacy.addAction(UIAction { _ in
            UIApplication.shared.open(URL(string: "https://playbit.app/help/privacy")!)
        }, for: .touchUpInside)
        let links = UIStackView(arrangedSubviews: [terms, privacy]); links.spacing = 20
        let stack = UIStackView(arrangedSubviews: [message, email, accountKey, code, action, resend, changeEmail, explanation, links])
        stack.axis = .vertical
        stack.spacing = 20
        stack.translatesAutoresizingMaskIntoConstraints = false
        let scroll = UIScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.keyboardDismissMode = .interactive
        scroll.addSubview(stack)
        view.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 32),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -32),
            stack.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor, constant: -40),
            email.heightAnchor.constraint(greaterThanOrEqualToConstant: 44),
            accountKey.heightAnchor.constraint(greaterThanOrEqualToConstant: 44),
            code.heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
        updateControls()
        prefillSavedAccount()
    }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        if replacementRequired { showReplacement(animated: false) } else { checkReplacement() }
    }
    deinit { NotificationCenter.default.removeObserver(self) }
    @objc private func checkReplacement() {
        guard !busy, !authenticated, !replacementRequired, !replacementStatusPending,
              navigationController?.topViewController === self else { return }
        replacementStatusPending = true
        EngineHost.shared.request(.status) { result in
            self.replacementStatusPending = false
            if case .success(let value) = result,
               (value["account_profile"] as? [String: Any])?["replacement_required"] as? Bool == true {
                self.showReplacement()
            }
        }
    }
    private func showReplacement(animated: Bool = true) {
        replacementRequired = true
        guard !replacementPresented, view.window != nil else { return }
        replacementPresented = true
        view.endEditing(true)
        title = "Playbit"
        view.subviews.forEach { $0.isHidden = true }
        navigationItem.hidesBackButton = true
        let alert = UIAlertController(title: "Replace Existing Data?",
            message: "This installation contains data for a different account. Continuing with signing in will replace that information.",
            preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Sign Out", style: .cancel) { _ in self.resolveReplacement(false) })
        let replace = UIAlertAction(title: "Replace", style: .default) { _ in self.resolveReplacement(true) }
        alert.addAction(replace)
        alert.preferredAction = replace
        present(alert, animated: animated)
    }
    private func resolveReplacement(_ replace: Bool) {
        EngineHost.shared.resolveReplacement(replace) { result in
            self.replacementPresented = false
            switch result {
            case .failure(let error):
                let alert = UIAlertController(title: "Couldn’t finish sign-in", message: error.localizedDescription, preferredStyle: .alert)
                alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in self.showReplacement() })
                self.present(alert, animated: true)
            case .success:
                self.replacementRequired = false
                if replace, let root = self.navigationController?.viewControllers.first, root !== self {
                    self.navigationController?.setViewControllers([root, self], animated: false)
                }
                self.view.subviews.forEach { $0.isHidden = false }
                self.navigationItem.hidesBackButton = false
                self.title = "Sign in"
                self.sent = false
                self.code.text = ""
                let key = self.accountKey.text?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                self.accountKey.text = ""
                self.authenticated = replace
                self.setBusy(false)
                if replace && !key.isEmpty {
                    self.setBusy(true)
                    EngineHost.shared.request(.recoverCredentials(key)) { result in
                        self.setBusy(false)
                        switch result {
                        case .success: self.finishEnrollment()
                        case .failure(let error): self.message.text = error.localizedDescription
                        }
                    }
                } else if replace { self.finishEnrollment() }
                else { self.message.text = "Enter your email address to continue." }
            }
        }
    }
    override func viewWillDisappear(_ animated: Bool) {
        // An abandoned or rejected key is not a credential to save in AutoFill.
        if isMovingFromParent {
            prefillAllowed = false

            if !authenticated { accountKey.text = "" }
        }
        super.viewWillDisappear(animated)
    }
    @objc private func submit() {
        guard !busy && action.isEnabled else { return }
        prefillAllowed = false
        if authenticated { finishEnrollment(); return }
        guard sent else {
            if accountKey.text?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty == false { signInWithKey() }
            else { requestCode() }
            return
        }
        sendingCode = false
        view.endEditing(true)
        setBusy(true)
        message.text = "Signing in…"
        EngineHost.shared.request(.signIn(code.text ?? "")) { result in
            switch result {
            case .success(let value):
                if value["replacement_required"] as? Bool == true {
                    self.setBusy(false)
                    self.showReplacement()
                    return
                }
                self.authenticated = true
                self.finishEnrollment(value["recovery"] as? [String: Any])
            case .failure(let error):
                self.setBusy(false)
                self.message.text = error.localizedDescription
            }
        }
    }
    private func prefillSavedAccount() {
        guard pb_ios_saved_account_available() != 0, !replacementRequired else { return }
        EngineHost.shared.request(.savedAccount) { [weak self] result in
            guard let self, self.prefillAllowed, !self.busy, !self.sent, !self.authenticated,
                  !self.replacementRequired, self.navigationController?.topViewController === self,
                  case .success(let value) = result,
                  let saved = value["saved_account"] as? [String: String] else { return }
            self.email.text = saved["email"]
            self.accountKey.text = saved["account_key"]
            self.updateControls()
        }
    }
    @objc private func edited() {
        prefillAllowed = false
        updateControls()
    }
    private func signInWithKey() {
        sendingCode = false
        view.endEditing(true)
        setBusy(true)
        let key = accountKey.text?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        EngineHost.shared.request(.signInWithKey(email: email.text ?? "", key: key)) { result in
            switch result {
            case .failure(let error): self.setBusy(false); self.message.text = error.localizedDescription
            case .success(let value):
                if value["replacement_required"] as? Bool == true {
                    self.setBusy(false)
                    self.showReplacement()
                    return
                }
                self.authenticated = true
                self.message.text = "Unlocking private data…"
                self.accountKey.text = ""
                EngineHost.shared.request(.recoverCredentials(key)) { restored in
                    switch restored {
                    case .success: self.finishEnrollment()
                    case .failure(let error): self.setBusy(false); self.message.text = "You’re signed in. \(error.localizedDescription)"
                    }
                }
            }
        }
    }
    private func finishEnrollment(_ known: [String: Any]? = nil) {
        if let known { presentRecovery(known); return }
        setBusy(true)
        message.text = "Signed in. Checking private access…"
        EngineHost.shared.request(.recoveryStatus) { result in
            self.setBusy(false)
            switch result {
            case .failure(let error):
                self.message.text = "You’re signed in. \(error.localizedDescription)"
                self.action.setTitle("Continue", for: .normal)
            case .success(let state): self.presentRecovery(state)
            }
        }
    }
    private func presentRecovery(_ state: [String: Any]) {
        setBusy(false)
        guard let navigation = navigationController else { return }

        if state["kind"] as? String == "ready" { AccountNameController.finish(from: self); return }
        let invitation = navigation.viewControllers.dropLast().last as? InvitationController
        let recovery = RecoveryController(state: state, allowInvitation: invitation != nil) { [weak invitation] in invitation?.recoveryPermitted = true }
        var controllers = navigation.viewControllers
        controllers.removeLast()
        controllers.append(recovery)
        navigation.setViewControllers(controllers, animated: true)
    }
    @objc private func requestCode() {
        guard !busy && Self.validEmail(email.text ?? "") else { return }
        sendingCode = true
        view.endEditing(true)
        setBusy(true)
        message.text = "Sending code…"
        EngineHost.shared.request(.requestCode(email.text?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "")) { result in
            self.setBusy(false)
            switch result {
            case .success:
                self.sent = true
                self.code.text = ""
                self.email.isEnabled = false
                self.code.isHidden = false
                self.resend.isHidden = false
                self.action.setTitle("Sign in", for: .normal)
                self.message.text = "Enter the six-digit code sent to \(self.email.text ?? "your email"). It expires after 10 minutes."
                self.updateControls()
                self.code.becomeFirstResponder()
            case .failure(let error): self.message.text = error.localizedDescription
            }
        }
    }
    private func setBusy(_ busy: Bool) {
        self.busy = busy
        updateControls()
    }
    @objc private func updateControls() {
        let ready = !busy && EngineHost.shared.channel != nil
        action.isEnabled = ready && (authenticated || (sent ? Self.validCode(code.text ?? "") : Self.validEmail(email.text ?? "")))
        action.setTitle(busy ? (sendingCode ? "Sending code…" : (authenticated ? "Checking private access…" : "Signing in…")) : (authenticated ? "Continue" : (sent ? "Sign in" : "Continue")), for: .normal)
        resend.isHidden = !sent || authenticated
        changeEmail.isHidden = !sent || authenticated
        resend.isEnabled = ready
        changeEmail.isEnabled = ready
        email.isEnabled = ready && !sent && !authenticated
        code.isEnabled = ready && !authenticated
        code.isHidden = !sent
        accountKey.isHidden = sent || authenticated
        accountKey.isEnabled = ready
    }
    @objc private func editEmail() {
        guard !busy && !authenticated else { return }

        sent = false
        code.text = ""
        message.text = "Enter your email address to continue."
        updateControls()
        email.becomeFirstResponder()
        email.selectedTextRange = email.textRange(from: email.beginningOfDocument, to: email.endOfDocument)
    }
    func textFieldShouldReturn(_ textField: UITextField) -> Bool {
        if action.isEnabled { submit() }
        return false
    }
    private static func validEmail(_ value: String) -> Bool {
        guard value.utf8.prefix(255).count <= 254 else { return false }
        let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
        let parts = value.split(separator: "@", omittingEmptySubsequences: false)
        return parts.count == 2 && !parts[0].isEmpty && !parts[1].isEmpty
            && value.rangeOfCharacter(from: .whitespacesAndNewlines.union(.controlCharacters)) == nil
    }
    private static func validCode(_ value: String) -> Bool {
        value.utf8.prefix(7).count == 6 && value.utf8.allSatisfy { $0 >= 48 && $0 <= 57 }
    }
}

final class RecoveryController: UIViewController, UITextViewDelegate {
    private var prefillAllowed = true
    private let state: [String: Any]
    private let allowInvitation: Bool
    private let onContinue: (() -> Void)?
    private let stack = UIStackView()
    private let message = UILabel()
    private let secret = UITextView()
    private let save = UIButton(type: .system)
    private let copy = UIButton(type: .system)
    private let retry = UIButton(type: .system)
    private let acknowledgment = UISwitch()
    private var setupState = RecoverySetup()
    private var working = false
    init(state: [String: Any], allowInvitation: Bool = false, onContinue: (() -> Void)? = nil) {
        self.state = state
        self.allowInvitation = allowInvitation
        self.onContinue = onContinue
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError() }
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Private data recovery"
        view.backgroundColor = .systemBackground
        let scroll = UIScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.keyboardDismissMode = .interactive
        stack.axis = .vertical
        stack.spacing = 20
        stack.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(stack)
        view.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -20),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 24),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -24),
            stack.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor, constant: -40)
        ])
        message.numberOfLines = 0
        message.accessibilityIdentifier = "recovery-status"
        stack.addArrangedSubview(message)
        switch state["kind"] as? String {
        case "setup": setup()
        case "locked": unlock()
        default:
            message.text = state["pending_save"] as? Bool == true
                ? "Your private credentials are available on this device. Their backup still needs to finish saving."
                : "Recovery is set up. Keep your account key somewhere safe."
            if state["pending_save"] as? Bool == true {
                save.configuration = .filled()
                save.setTitle("Retry backup", for: .normal)
                save.addTarget(self, action: #selector(saveSecret), for: .touchUpInside)
                stack.addArrangedSubview(save)
            }
            stack.addArrangedSubview(button("Edit display name", #selector(editDisplayName)))
            stack.addArrangedSubview(button("Done", #selector(done)))
        }
    }
    private func setup() {
        let explanation = UILabel()
        explanation.text = "Save your account key. Playbit never receives it. If you lose every copy of this key, signing in alone cannot recover your private data."
        explanation.numberOfLines = 0
        stack.insertArrangedSubview(explanation, at: 0)
        configureSecret(editable: false)
        stack.addArrangedSubview(secret)
        copy.configuration = .tinted()
        copy.setTitle("Copy account key", for: .normal)
        copy.accessibilityIdentifier = "recovery-copy"
        copy.addTarget(self, action: #selector(copySecret), for: .touchUpInside)
        stack.addArrangedSubview(copy)
        retry.configuration = .tinted()
        retry.setTitle("Retry preparation", for: .normal)
        retry.accessibilityIdentifier = "recovery-retry"
        retry.addTarget(self, action: #selector(prepareSecret), for: .touchUpInside)
        stack.addArrangedSubview(retry)
        acknowledgment.accessibilityIdentifier = "recovery-saved"
        acknowledgment.accessibilityLabel = "I’ve saved my account key"
        acknowledgment.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.setupState.acknowledge(self.acknowledgment.isOn)
            self.updateSetupControls()
        }, for: .valueChanged)
        let label = UILabel()
        label.text = "I’ve saved my account key"
        label.numberOfLines = 0
        let row = UIStackView(arrangedSubviews: [acknowledgment, label])
        row.spacing = 12
        stack.addArrangedSubview(row)
        save.configuration = .filled()
        save.setTitle("Save and continue", for: .normal)
        save.accessibilityIdentifier = "recovery-save"
        save.isEnabled = false
        save.addTarget(self, action: #selector(saveSecret), for: .touchUpInside)
        stack.addArrangedSubview(save)
        if let prepared = state["secret"] as? String {
            _ = setupState.beginPreparation()
            setupState.prepared(prepared)
            secret.text = setupState.secret
            updateSetupControls()
        } else {
            prepareSecret()
        }
    }
    private func updateSetupControls() {
        copy.isEnabled = setupState.canCopy
        acknowledgment.isEnabled = setupState.canAcknowledge
        acknowledgment.isOn = setupState.acknowledged
        save.isEnabled = setupState.canSave
        retry.isHidden = !setupState.canRetry
        switch setupState.phase {
        case .preparing: message.text = "Preparing account key…"
        case .saving: message.text = "Saving recovery backup…"
        default: message.text = setupState.error
        }
        message.isHidden = message.text == nil
    }
    @objc private func prepareSecret() {
        guard setupState.beginPreparation() else { return }
        updateSetupControls()
        EngineHost.shared.request(.prepareRecovery) { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let value): self.setupState.prepared(value["secret"] as? String)
            case .failure(let error): self.setupState.preparationFailed(error.localizedDescription)
            }
            self.secret.text = self.setupState.secret
            self.updateSetupControls()
        }
    }
    private func unlock() {
        message.text = "You’re signed in. Enter your saved account key to unlock existing private data. Email sign-in does not recover private keys."
        configureSecret(editable: true)
        stack.addArrangedSubview(secret)
        stack.addArrangedSubview(button("Recover private data", #selector(recoverSecret)))
        secret.delegate = self
        prefillSavedAccount()
        if allowInvitation {
            let explanation = UILabel()
            explanation.text = "This invitation can unlock its shared data. Your other private data will stay locked, and your existing backup will be preserved."
            explanation.numberOfLines = 0
            explanation.font = .preferredFont(forTextStyle: .footnote)
            stack.addArrangedSubview(explanation)
            stack.addArrangedSubview(button("Join this invitation", #selector(done)))
        }
    }
    private func configureSecret(editable: Bool) {
        secret.isEditable = editable
        secret.isScrollEnabled = false
        secret.font = .monospacedSystemFont(ofSize: 16, weight: .regular)
        secret.backgroundColor = .secondarySystemBackground
        secret.autocorrectionType = .no
        secret.autocapitalizationType = .none
        secret.accessibilityLabel = "Account key"
        secret.accessibilityIdentifier = "recovery-secret"
        secret.heightAnchor.constraint(greaterThanOrEqualToConstant: 110).isActive = true
    }
    private func button(_ title: String, _ selector: Selector) -> UIButton {
        let button = UIButton(type: .system)
        button.configuration = .tinted()
        button.setTitle(title, for: .normal)
        button.addTarget(self, action: selector, for: .touchUpInside)
        return button
    }
    @objc private func editDisplayName() { navigationController?.pushViewController(AccountNameController(), animated: true) }
    @objc private func copySecret() {
        guard let value = setupState.secret else { return }
        UIPasteboard.general.string = value
    }
    @objc private func saveSecret() {
        guard !working else { return }
        let settingUp = state["kind"] as? String == "setup"
        if settingUp {
            guard setupState.beginSave() else { return }
            updateSetupControls()
        } else {
            guard state["pending_save"] as? Bool == true else { return }
            message.text = "Saving recovery backup…"
        }
        working = true
        save.isEnabled = false
        EngineHost.shared.request(.saveRecovery) { [weak self] result in
            guard let self else { return }
            self.working = false
            switch result {
            case .success:
                if self.viewIfLoaded?.window != nil { self.done() }
            case .failure(let error):
                if settingUp {
                    self.setupState.saveFailed(error.localizedDescription)
                    self.updateSetupControls()
                } else {
                    self.message.text = error.localizedDescription
                    self.save.isEnabled = true
                }
            }
        }
    }
    private func prefillSavedAccount() {
        guard pb_ios_saved_account_available() != 0 else { return }
        EngineHost.shared.request(.savedAccount) { [weak self] result in
            guard let self, self.prefillAllowed, !self.working,
                  self.navigationController?.topViewController === self,
                  case .success(let value) = result,
                  let saved = value["saved_account"] as? [String: String] else { return }
            self.secret.text = saved["account_key"]
        }
    }
    func textViewDidChange(_ textView: UITextView) { prefillAllowed = false }
    @objc private func recoverSecret() {
        guard !working else { return }
        prefillAllowed = false
        working = true
        view.endEditing(true)
        EngineHost.shared.request(.recoverCredentials(secret.text ?? "")) { result in
            self.working = false
            switch result {
            case .success: self.done()
            case .failure(let error): self.message.text = error.localizedDescription
            }
        }
    }
    @objc private func done() { AccountNameController.finish(from: self, onContinue: onContinue) }
}

final class PersonalVolumesController: UITableViewController {
    private static let performance = OSLog(subsystem: "app.playbit", category: "PersonalConsent")
    private let session: String
    private var access: PersonalVolumeAccess?
    private var saveError: String?
    private let message = UILabel()
    init(session: String) {
        self.session = session
        super.init(style: .insetGrouped)
    }
    required init?(coder: NSCoder) { fatalError() }
    override func viewDidLoad() {
        let softwareStart = pb_ios_trace_start()
        defer { pb_ios_trace_end(PB_IOS_TRACE_CONSENT_CONSTRUCTION, softwareStart) }
        os_signpost(.begin, log: Self.performance, name: "Consent construction")
        defer { os_signpost(.end, log: Self.performance, name: "Consent construction") }
        super.viewDidLoad()
        title = "Personal volumes"
        message.numberOfLines = 0; message.textAlignment = .center; message.textColor = .secondaryLabel
        message.font = .preferredFont(forTextStyle: .body); message.adjustsFontForContentSizeCategory = true
        message.accessibilityIdentifier = "personal-access-status"
        tableView.backgroundView = message
        tableView.register(PersonalVolumeCell.self, forCellReuseIdentifier: "personal-volume")
        loadChoices()
    }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        showSaveError()
    }
    @objc private func loadChoices() {
        guard access == nil else { return }
        navigationItem.rightBarButtonItem = UIBarButtonItem(title: "Save", style: .done, target: self, action: #selector(saveChoices))
        navigationItem.rightBarButtonItem?.isEnabled = false
        message.text = "Loading volumes…"
        EngineHost.shared.request(.personalVolumes(session)) { [weak self] result in
            guard let self else { return }
            let softwareStart = pb_ios_trace_start()
            defer { pb_ios_trace_end(PB_IOS_TRACE_CONSENT_SNAPSHOT, softwareStart) }
            os_signpost(.begin, log: Self.performance, name: "Consent snapshot")
            defer { os_signpost(.end, log: Self.performance, name: "Consent snapshot") }
            do {
                self.access = try PersonalVolumeAccess(result.get())
                self.message.text = self.access!.rows.isEmpty ? "No personal volumes are available on this device." : nil
                self.updateSaveState()
                self.tableView.reloadData()
            } catch {
                self.message.text = Self.errorText(error)
                self.navigationItem.rightBarButtonItem = UIBarButtonItem(title: "Retry", style: .plain, target: self, action: #selector(self.loadChoices))
            }
        }
    }
    private static func errorText(_ error: Error) -> String {
        // Leave room for the replacement scalar if a UTF-8 prefix ends mid-codepoint.
        String(decoding: error.localizedDescription.utf8.prefix(4093), as: UTF8.self)
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { access?.rows.count ?? 0 }
    override func tableView(_ tableView: UITableView, titleForFooterInSection section: Int) -> String? {
        "Selected volumes are available to this session across app updates. Choose Off to revoke access. Shared session data is separate."
    }
    override func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let softwareStart = pb_ios_trace_start()
        defer { pb_ios_trace_end(PB_IOS_TRACE_CONSENT_CELL, softwareStart) }
        os_signpost(.begin, log: Self.performance, name: "Consent cell")
        defer { os_signpost(.end, log: Self.performance, name: "Consent cell") }
        let cell = tableView.dequeueReusableCell(withIdentifier: "personal-volume", for: indexPath) as! PersonalVolumeCell
        let row = access!.rows[indexPath.row]
        cell.configure(row, enabled: access!.phase == .ready) { [weak self] choice in
            let softwareStart = pb_ios_trace_start()
            defer { pb_ios_trace_end(PB_IOS_TRACE_CONSENT_CHOICE, softwareStart) }
            os_signpost(.begin, log: Self.performance, name: "Consent choice")
            defer { os_signpost(.end, log: Self.performance, name: "Consent choice") }
            self?.access?.change(row.id, to: choice)
        }
        return cell
    }
    private func updateSaveState() {
        let ready = access?.phase == .ready
        navigationItem.rightBarButtonItem?.title = access?.phase == .saving ? "Saving…" : "Save"
        navigationItem.rightBarButtonItem?.isEnabled = ready
        for cell in tableView.visibleCells { (cell as? PersonalVolumeCell)?.control.isEnabled = ready }
    }
    private func showSaveError() {
        guard navigationController?.topViewController === self, presentedViewController == nil,
              let text = saveError else { return }
        saveError = nil
        let alert = UIAlertController(title: "Couldn’t save access", message: text, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default)); present(alert, animated: true)
    }
    @objc private func saveChoices() {
        guard let grants = access?.beginSave() else { return }
        updateSaveState()
        let saveID = OSSignpostID(log: Self.performance)
        os_signpost(.begin, log: Self.performance, name: "Consent save", signpostID: saveID)
        EngineHost.shared.request(.selectPersonalVolumes(session: session, grants: grants)) { [weak self] result in
            os_signpost(.end, log: Self.performance, name: "Consent save", signpostID: saveID)
            guard let self else { return }
            let softwareStart = pb_ios_trace_start()
            defer { pb_ios_trace_end(PB_IOS_TRACE_CONSENT_RESPONSE, softwareStart) }
            os_signpost(.begin, log: Self.performance, name: "Consent response")
            defer { os_signpost(.end, log: Self.performance, name: "Consent response") }
            switch result {
            case .failure(let error):
                guard self.access?.finishSave(success: false) == true else { return }
                self.updateSaveState()
                self.saveError = Self.errorText(error)
                self.showSaveError()
            case .success:
                guard self.access?.finishSave(success: true) == true else { return }
                // Navigation may have removed this chooser while the durable
                // save was pending. Its result cannot navigate or retry a different app.
                guard self.navigationController?.topViewController === self else { return }
                self.navigationController?.popViewController(animated: true)
            }
        }
    }
}

private final class PersonalVolumeCell: UITableViewCell {
    private let name = UILabel()
    let control = UISegmentedControl(items: ["Off", "Read", "Read/write"])
    private var onChange: ((Int) -> Void)?
    override init(style: UITableViewCell.CellStyle, reuseIdentifier: String?) {
        super.init(style: style, reuseIdentifier: reuseIdentifier)
        name.numberOfLines = 0
        name.font = .preferredFont(forTextStyle: .body); name.adjustsFontForContentSizeCategory = true
        control.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.onChange?(self.control.selectedSegmentIndex)
        }, for: .valueChanged)
        let row = UIStackView(arrangedSubviews: [name, control]); row.axis = .vertical; row.spacing = 10
        row.translatesAutoresizingMaskIntoConstraints = false; contentView.addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: contentView.layoutMarginsGuide.leadingAnchor),
            row.trailingAnchor.constraint(equalTo: contentView.layoutMarginsGuide.trailingAnchor),
            row.topAnchor.constraint(equalTo: contentView.topAnchor, constant: 12),
            row.bottomAnchor.constraint(equalTo: contentView.bottomAnchor, constant: -12)
        ])
        selectionStyle = .none
    }
    required init?(coder: NSCoder) { fatalError() }
    func configure(_ row: PersonalVolumeAccess.Row, enabled: Bool, onChange: @escaping (Int) -> Void) {
        name.text = row.name; control.accessibilityLabel = row.name
        control.selectedSegmentIndex = row.choice; control.isEnabled = enabled
        self.onChange = onChange
    }
}
