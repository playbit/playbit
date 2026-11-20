import UIKit
import os

/// Published apps supply their authenticated app identity and current build. The catalog is
/// paged through AP only on explicit actions, independent of local session use.
final class NewSessionController: UIViewController, UITableViewDataSource, UITableViewDelegate {
    private let tableView = UITableView(frame: .zero, style: .plain)
    private let instruction = UILabel()
    private let feedback = UILabel()
    private let footer = UIView()
    private let enrolled: Bool
    private let examples: [CuratedExample]
    private let exampleError: String?
    private let onPreview: ([String: Any]) -> Void
    private let onSession: (String) -> Void
    private let onSignIn: () -> Void
    private var page = AppPage()
    private var after: String?
    private var pending = false
    private var creating = false
    private var openingExample = false
    private var closed = false
    private var message: String?
    private var exampleMessage: String?
    private let artwork = SessionArtwork(profile: EngineHost.shared.profile, published: true)
    private var displayNames: [String: String] = [:]
    private var displayErrors: [String: String] = [:]
    private var displayRequested = Set<String>()
    private var displayInFlight = false
    private var displayGeneration: UInt64 = 0
    private static let performance = OSLog(subsystem: "app.playbit", category: "NewSession")

    init(enrolled: Bool, examples: [CuratedExample], exampleError: String?, onPreview: @escaping ([String: Any]) -> Void,
         onSession: @escaping (String) -> Void, onSignIn: @escaping () -> Void) {
        self.enrolled = enrolled; self.onPreview = onPreview
        self.examples = examples; self.exampleError = exampleError
        self.onSession = onSession; self.onSignIn = onSignIn
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        artwork.changed = { [weak self] build in self?.updateDisplayRows(build) }
        title = "New Session"
        view.backgroundColor = .systemBackground
        navigationItem.leftBarButtonItem = UIBarButtonItem(barButtonSystemItem: .close, target: self, action: #selector(close))
        navigationItem.leftBarButtonItem?.accessibilityIdentifier = "new-session-close"
        navigationItem.leftBarButtonItem?.accessibilityLabel = "Close New Session"
        if enrolled {
            navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .refresh, target: self, action: #selector(refresh))
            navigationItem.rightBarButtonItem?.accessibilityLabel = "Refresh apps"
            navigationItem.rightBarButtonItem?.accessibilityIdentifier = "new-session-refresh"
        }
        instruction.text = "Try a shared example or select an app to launch"
        instruction.font = .preferredFont(forTextStyle: .body)
        instruction.adjustsFontForContentSizeCategory = true
        instruction.textAlignment = .center
        instruction.numberOfLines = 0
        instruction.accessibilityIdentifier = "new-session-instruction"
        tableView.dataSource = self; tableView.delegate = self
        tableView.rowHeight = UITableView.automaticDimension
        tableView.estimatedRowHeight = 58
        tableView.separatorStyle = .none
        tableView.sectionHeaderTopPadding = 0
        tableView.backgroundColor = .systemBackground
        tableView.contentInset.bottom = 16
        tableView.accessibilityIdentifier = "new-session-list"
        tableView.accessibilityLabel = "Apps"
        feedback.font = .preferredFont(forTextStyle: .footnote)
        feedback.adjustsFontForContentSizeCategory = true
        feedback.textColor = .secondaryLabel
        feedback.numberOfLines = 0
        feedback.accessibilityIdentifier = "new-session-feedback"
        footer.addSubview(feedback)
        tableView.tableFooterView = footer
        instruction.translatesAutoresizingMaskIntoConstraints = false
        tableView.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(instruction); view.addSubview(tableView)
        NSLayoutConstraint.activate([
            instruction.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 8),
            instruction.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 18),
            instruction.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -18),
            tableView.topAnchor.constraint(equalTo: instruction.bottomAnchor, constant: 12),
            tableView.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            tableView.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor),
            tableView.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor),
        ])
        if enrolled { load(after: nil) } else { update() }
    }
    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        let start = pb_ios_trace_start()
        defer { pb_ios_trace_end(PB_IOS_TRACE_NEW_SESSION_LAYOUT, start) }
        let width = tableView.bounds.width
        let textHeight = feedback.text == nil ? 0 : feedback.sizeThatFits(
            CGSize(width: max(0, width - 36), height: .greatestFiniteMagnitude)).height
        feedback.frame = CGRect(x: 18, y: 8, width: max(0, width - 36), height: textHeight)
        let footerHeight = textHeight == 0 ? 0 : textHeight + 16
        if footer.frame.width != width || footer.frame.height != footerHeight {
            footer.frame = CGRect(x: 0, y: 0, width: width, height: footerHeight)
            tableView.tableFooterView = footer
        }
        // UIKit materializes only visible cells. Insets position short pages at
        // the bottom while long pages keep native scrolling and selection.
        let top = max(0, tableView.bounds.height - tableView.contentInset.bottom - tableView.contentSize.height)
        if abs(tableView.contentInset.top - top) > 0.5 {
            let wasAtTop = tableView.contentOffset.y <= -tableView.adjustedContentInset.top + 1
            tableView.contentInset.top = top
            if wasAtTop && !tableView.isDragging && !tableView.isDecelerating {
                tableView.contentOffset.y = -tableView.adjustedContentInset.top
            }
        }
    }
    @objc private func close() { closed = true; dismiss(animated: true) }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        loadNextDisplay()
    }
    @objc private func refresh() { load(after: after) }
    private func visibleResult() -> Bool {
        !closed && navigationController?.presentingViewController != nil && navigationController?.isBeingDismissed != true
    }
    private func update(resetScroll: Bool = false) {
        let start = pb_ios_trace_start()
        defer { pb_ios_trace_end(PB_IOS_TRACE_NEW_SESSION_CONTROLS, start) }
        os_signpost(.begin, log: Self.performance, name: "App controls")
        defer { os_signpost(.end, log: Self.performance, name: "App controls") }
        navigationItem.rightBarButtonItem?.isEnabled = !pending && !openingExample
        feedback.text = [exampleMessage, message, exampleError,
            enrolled ? nil : "Sign in to see your apps. Your saved sessions remain available offline."]
            .compactMap { $0 }.joined(separator: "\n")
        tableView.reloadData()
        if resetScroll {
            tableView.scrollToRow(at: IndexPath(row: 0, section: 0), at: .top, animated: false)
        }
        view.setNeedsLayout()
    }
    private func load(after cursor: String?) {
        guard !pending && !closed else { return }
        pending = true; message = "Loading apps…"; update()
        EngineHost.shared.request(.listApps(cursor)) { [weak self] result in
            guard let self else { return }
            self.pending = false
            guard self.visibleResult() else { return }
            let previousAfter = self.after
            switch result {
            case .success(let value):
                do {
                    let log = Self.performance
                    os_signpost(.begin, log: log, name: "App page")
                    defer { os_signpost(.end, log: log, name: "App page") }
                    self.page = try AppPage(value, after: cursor)
                    self.displayGeneration &+= 1
                    self.displayNames.removeAll(); self.displayErrors.removeAll(); self.displayRequested.removeAll()
                    self.artwork.reconcile(builds: [])
                    self.after = cursor
                    self.message = self.page.rows.isEmpty ? (cursor == nil ? "You haven’t published any apps yet." : "No apps on this page.") : nil
                } catch { self.message = error.localizedDescription }
            case .failure(let error): self.message = "Couldn’t load apps. \(error.localizedDescription)"
            }
            self.update(resetScroll: self.after != previousAfter)
            self.loadNextDisplay()
        }
    }
    private func updateDisplayRows(_ build: String) {
        guard visibleResult() else { return }
        let start = pb_ios_trace_start()
        defer { pb_ios_trace_end(PB_IOS_TRACE_NEW_SESSION_CONTROLS, start) }
        let paths = (tableView.indexPathsForVisibleRows ?? []).filter {
            $0.section == 1 && $0.row < page.rows.count && page.rows[$0.row].build == build
        }
        if !paths.isEmpty { tableView.reconfigureRows(at: paths) }
        view.setNeedsLayout()
    }
    private func loadNextDisplay() {
        guard !pending && !displayInFlight && visibleResult(),
              navigationController?.topViewController === self,
              let row = page.rows.first(where: { !displayRequested.contains($0.build) }) else { return }
        let build = row.build
        displayRequested.insert(build); displayInFlight = true
        let generation = displayGeneration
        EngineHost.shared.request(.getAppDisplay(publisher: row.publisher, appID: row.appID, build: build)) { [weak self] result in
            guard let self else { return }
            self.displayInFlight = false
            guard self.visibleResult() else { return }
            if self.displayGeneration == generation {
                switch result {
                case .success(let value):
                    if value["build_id"] as? String == build, let name = value["name"] as? String,
                       !name.isEmpty, name.utf8.count <= 256 {
                        self.displayNames[build] = name
                        self.artwork.reconcile(builds: self.page.rows.map(\.build).filter { self.displayNames[$0] != nil })
                    } else { self.displayErrors[build] = "App details are invalid. Refresh to retry." }
                case .failure:
                    self.displayErrors[build] = "App details unavailable. Refresh to retry."
                }
                self.updateDisplayRows(build)
            }
            self.loadNextDisplay()
        }
    }
    private func create(_ row: AppPage.Row) {
        guard !pending && !closed && !openingExample && row.unavailable == nil else { return }
        pending = true; creating = true
        message = "Creating a session for \(row.label)… You can close this sheet; a created session will remain in Sessions."
        update()
        EngineHost.shared.request(.createAppSession(publisher: row.publisher, appID: row.appID)) { [weak self] result in
            guard let self else { return }
            self.pending = false; self.creating = false
            guard self.visibleResult(), self.navigationController?.topViewController === self else { return }
            switch result {
            case .success(let value):
                guard let id = value["id"] as? String, Identifier.valid(id, bytes: 32) else {
                    self.message = "The session result is invalid. Check Sessions before trying again."
                    self.update(); return
                }
                self.closed = true
                self.onSession(id)
            case .failure(let error):
                self.message = "Couldn’t create the session. \(error.localizedDescription)"
                self.update()
                self.loadNextDisplay()
            }
        }
    }
    private func openExample(_ example: CuratedExample) {
        guard !closed && !creating && !openingExample else { return }
        openingExample = true
        exampleMessage = "Opening \(example.name)…"
        update()
        EngineHost.shared.request(.previewInvitation(example.invitation)) { [weak self] result in
            guard let self else { return }
            self.openingExample = false
            guard self.visibleResult(), self.navigationController?.topViewController === self else { return }
            switch result {
            case .success(let preview): self.closed = true; self.onPreview(preview)
            case .failure(let error):
                self.exampleMessage = "Couldn’t open \(example.name). \(error.localizedDescription) You can retry or open a saved session."
                self.update()
            }
        }
    }
    func numberOfSections(in tableView: UITableView) -> Int { 4 }
    func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        switch section {
        case 0: return 1
        case 3: return examples.count
        case 1: return enrolled ? page.rows.count : 1
        default: return enrolled ? (page.next == nil ? 0 : 1) + (after == nil ? 0 : 1) : 0
        }
    }
    func tableView(_ tableView: UITableView, heightForHeaderInSection section: Int) -> CGFloat {
        section == 1 || section == 3 && !examples.isEmpty ? 18 : 0
    }
    func tableView(_ tableView: UITableView, viewForHeaderInSection section: Int) -> UIView? {
        guard section == 1 || section == 3 && !examples.isEmpty else { return nil }
        let container = UIView()
        let separator = UIView()
        separator.backgroundColor = .separator
        separator.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(separator)
        NSLayoutConstraint.activate([
            separator.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: 18),
            separator.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -18),
            separator.centerYAnchor.constraint(equalTo: container.centerYAnchor),
            separator.heightAnchor.constraint(equalToConstant: 1 / max(1, traitCollection.displayScale)),
        ])
        return container
    }
    func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let cell = tableView.dequeueReusableCell(withIdentifier: "app-row")
            ?? UITableViewCell(style: .subtitle, reuseIdentifier: "app-row")
        var content = UIListContentConfiguration.subtitleCell()
        content.textProperties.font = UIFontMetrics(forTextStyle: .title3)
            .scaledFont(for: .systemFont(ofSize: 20, weight: .semibold))
        content.textProperties.numberOfLines = 0
        content.secondaryTextProperties.font = .preferredFont(forTextStyle: .subheadline)
        content.secondaryTextProperties.color = .secondaryLabel
        content.secondaryTextProperties.numberOfLines = 0
        content.imageProperties.maximumSize = CGSize(width: 32, height: 32)
        content.imageProperties.reservedLayoutSize = CGSize(width: 40, height: 40)
        content.imageProperties.tintColor = .label
        content.imageToTextPadding = 16
        content.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 8, leading: 18, bottom: 8, trailing: 18)
        cell.backgroundColor = .systemBackground
        cell.accessoryType = .none; cell.accessoryView = nil; cell.accessibilityTraits = .button
        cell.isUserInteractionEnabled = !pending && !openingExample
        cell.selectionStyle = pending ? .none : .default
        if path.section == 0 {
            content.text = "Join Session"
            content.image = UIImage(named: "JoinSession")
            cell.accessoryType = .disclosureIndicator
            cell.accessibilityIdentifier = "new-session-join"
            cell.isUserInteractionEnabled = !creating && !openingExample
        } else if path.section == 3 {
            let example = examples[path.row]
            content.text = "Open \(example.name)"
            content.secondaryText = example.description
            content.image = UIImage(systemName: "checklist")
            cell.accessibilityIdentifier = "example-open:" + example.id
            cell.isUserInteractionEnabled = !creating && !openingExample
            let source = UIButton(type: .system)
            var sourceStyle = UIButton.Configuration.plain()
            sourceStyle.title = "Source"
            sourceStyle.subtitle = "GitHub access"
            source.configuration = sourceStyle
            source.accessibilityLabel = "Source and build instructions for \(example.name); GitHub repository access required"
            source.accessibilityIdentifier = "example-source:" + example.id
            source.frame = CGRect(x: 0, y: 0, width: 108, height: 54)
            source.addAction(UIAction { [weak self] _ in
                UIApplication.shared.open(example.source) { opened in
                    guard let self, self.visibleResult(), !opened else { return }
                    self.exampleMessage = "Couldn’t open the source link. Try again when your browser is available."
                    self.update()
                }
            }, for: .touchUpInside)
            cell.accessoryView = source
        } else if path.section == 1 && !enrolled {
            content.text = "Sign in"
            cell.accessoryType = .disclosureIndicator
            cell.accessibilityIdentifier = "new-session-sign-in"
        } else if path.section == 1 {
            let row = page.rows[path.row]
            let name = displayNames[row.build]
            content.text = name ?? row.label
            var details: [String] = []
            if let name, name != row.label { details.append(row.label) }
            if let unavailable = row.unavailable { details.append(unavailable) }
            if let error = displayErrors[row.build] { details.append(error) }
            else if artwork.failed(row.build) { details.append("App artwork unavailable.") }
            content.secondaryText = details.isEmpty ? nil : details.joined(separator: " · ")
            content.image = artwork.image(row.build)
            if content.image == nil { content.directionalLayoutMargins.leading += 56 }
            content.imageProperties.maximumSize = CGSize(width: 40, height: 40)
            content.imageProperties.tintColor = nil
            cell.accessibilityIdentifier = "app-row:" + row.id
            cell.isUserInteractionEnabled = !pending && !openingExample && row.unavailable == nil
            if row.unavailable != nil { cell.selectionStyle = .none }
        } else {
            let title = path.row == 0 && page.next != nil ? "Next page" : "First page"
            content.text = title
            cell.accessibilityIdentifier = "app-page-" + title
        }
        if !cell.isUserInteractionEnabled {
            cell.selectionStyle = .none
            content.textProperties.color = .secondaryLabel
            cell.accessibilityTraits.insert(.notEnabled)
        } else { cell.selectionStyle = .default }
        cell.contentConfiguration = content
        return cell
    }
    func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !closed else { return }
        if path.section == 3 { openExample(examples[path.row]) }
        else if path.section == 0 && !creating && !openingExample {
            navigationController?.pushViewController(InvitationEntryController(onPreview: onPreview), animated: true)
        } else if !pending && !openingExample {
            if !enrolled { closed = true; onSignIn() }
            else if path.section == 1 { create(page.rows[path.row]) }
            else if path.row == 0, let next = page.next { load(after: next) }
            else { load(after: nil) }
        }
    }
}
