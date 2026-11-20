import UIKit
import os

/// Account settings use the same local owner and encrypted account outbox as
/// invitations and personal consent. UIKit owns editing and marked text.
final class AccountNameController: UIViewController, UITextFieldDelegate {
    private let field = UITextField()
    private let message = UILabel()
    private let save = UIButton(type: .system)
    private let backup = UILabel()
    private let retryBackup = UIButton(type: .system)
    private let onSaved: (() -> Void)?
    private let onlyIfMissing: Bool
    private var working = false
    private var refreshing = false
    private var refreshNeeded = false
    private var loaded = false
    private var available = false
    private var failure: String?
    private var loadFailure: String?
    private static let performance = OSLog(subsystem: "app.playbit", category: "AccountName")

    init(onlyIfMissing: Bool = false, onSaved: (() -> Void)? = nil) {
        self.onlyIfMissing = onlyIfMissing
        self.onSaved = onSaved
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError() }
    deinit { NotificationCenter.default.removeObserver(self) }
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Display name"
        view.backgroundColor = .systemBackground
        field.placeholder = "Display name"
        field.borderStyle = .roundedRect
        field.font = .preferredFont(forTextStyle: .body)
        field.adjustsFontForContentSizeCategory = true
        field.textContentType = .nickname
        field.returnKeyType = .done
        field.enablesReturnKeyAutomatically = true
        field.delegate = self
        field.accessibilityIdentifier = "account-display-name"
        field.addTarget(self, action: #selector(edited), for: .editingChanged)
        message.numberOfLines = 0
        message.font = .preferredFont(forTextStyle: .body)
        message.adjustsFontForContentSizeCategory = true
        save.configuration = .filled()
        save.setTitle("Save display name", for: .normal)
        save.accessibilityIdentifier = "account-display-name-save"
        save.addTarget(self, action: #selector(submit), for: .touchUpInside)
        let explanation = UILabel()
        explanation.text = "Hello friend and welcome. Please tell us what we should call you. You can change your display name later in account settings."
        explanation.numberOfLines = 0
        explanation.font = .preferredFont(forTextStyle: .body)
        explanation.adjustsFontForContentSizeCategory = true
        backup.numberOfLines = 0
        backup.font = .preferredFont(forTextStyle: .footnote)
        backup.adjustsFontForContentSizeCategory = true
        retryBackup.setTitle("Retry saving account key", for: .normal)
        retryBackup.addTarget(self, action: #selector(retryAccountKeyBackup), for: .touchUpInside)
        retryBackup.isHidden = true
        let stack = UIStackView(arrangedSubviews: [field, message, save, explanation, backup, retryBackup])
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
            field.heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
        NotificationCenter.default.addObserver(self, selector: #selector(refresh), name: EngineHost.backgroundChanged, object: nil)
        updateControls()
        refresh()
    }
    @objc private func refresh() {
        guard !refreshing else { refreshNeeded = true; return }
        refreshing = true
        EngineHost.shared.request(.status) { result in
            self.refreshing = false
            defer { if self.refreshNeeded { self.refreshNeeded = false; self.refresh() } }
            switch result {
            case .success(let state):
                self.backup.isHidden = pb_ios_saved_account_available() == 0
                let backupState = state["account_key_backup"] as? String ?? "unavailable"
                self.backup.text = ["unavailable": "Account key recovery copy unavailable", "pending": "Saving account key recovery copy…", "saved": "Account key recovery copy saved", "failed": "Couldn’t save the account key recovery copy. Your local account is ready."][backupState]
                self.retryBackup.isHidden = backupState != "failed" || self.backup.isHidden
                self.loadFailure = nil
                self.navigationItem.rightBarButtonItem = nil
                let profile = state["account_profile"] as? [String: Any] ?? [:]
                let name = profile["display_name"] as? String
                self.available = profile["unlocked"] as? Bool == true && (name != nil || profile["initialized"] as? Bool == true)
                if !self.loaded && self.available {
                    self.loaded = true
                    // Recovery can finish before the account's initial live
                    // projection. An existing name completes onboarding; it
                    // must not require a redundant save or interrupt editing.
                    if self.onlyIfMissing && name != nil {
                        guard self.navigationController?.topViewController === self else { return }
                        self.onSaved?()
                        self.navigationController?.popViewController(animated: true)
                        return
                    }
                    self.field.text = name ?? ""
                }
                self.updateControls()
            case .failure(let error):
                self.loadFailure = error.localizedDescription
                self.navigationItem.rightBarButtonItem = UIBarButtonItem(title: "Retry", style: .plain,
                    target: self, action: #selector(self.refresh))
                self.updateControls()
            }
        }
    }
    @objc private func updateControls() {
        os_signpost(.begin, log: Self.performance, name: "Display name controls")
        defer { os_signpost(.end, log: Self.performance, name: "Display name controls") }
        field.isEnabled = available && !working
        let valid = Self.valid(field.text ?? "")
        save.isEnabled = available && !working && field.markedTextRange == nil && valid
        save.setTitle(working ? "Saving display name…" : "Save display name", for: .normal)
        let invalid = (field.text ?? "").utf8.prefix(257).count > 256
            ? "Use a shorter display name." : "Enter a display name using one line of text."
        message.text = failure ?? loadFailure ?? (!available ? "Your account data is still loading. Your existing name will be preserved."
            : (!valid && !(field.text ?? "").isEmpty ? invalid : nil))
    }
    @objc private func retryAccountKeyBackup() {
        guard retryBackup.isEnabled else { return }
        retryBackup.isEnabled = false
        EngineHost.shared.request(.retryAccountKeyBackup) { result in
            self.retryBackup.isEnabled = true
            if case .failure(let error) = result { self.backup.text = error.localizedDescription }
            else { self.refresh() }
        }
    }
    @objc private func edited() { failure = nil; updateControls() }
    private static func valid(_ value: String) -> Bool {
        os_signpost(.begin, log: Self.performance, name: "Display name validation")
        defer { os_signpost(.end, log: Self.performance, name: "Display name validation") }
        guard value.utf8.prefix(257).count <= 256 else { return false }
        let name = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return !name.isEmpty && name.rangeOfCharacter(from: .controlCharacters) == nil
    }
    @objc private func submit() {
        guard save.isEnabled && !working else { return }
        working = true
        failure = nil
        updateControls()
        let name = (field.text ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        EngineHost.shared.request(.setDisplayName(name)) { result in
            self.working = false
            self.updateControls()
            switch result {
            case .success:
                guard self.navigationController?.topViewController === self else { return }
                self.onSaved?()
                self.navigationController?.popViewController(animated: true)
            case .failure(let error): self.failure = error.localizedDescription; self.updateControls()
            }
        }
    }
    func textFieldShouldReturn(_ textField: UITextField) -> Bool { submit(); return false }

    /// Recovery precedes private profile projection. Never replace an existing
    /// profile with an empty local default while its first changeset is loading.
    static func finish(from controller: UIViewController, onContinue: (() -> Void)? = nil) {
        guard let navigation = controller.navigationController else { return }
        let expectedTop = navigation.topViewController
        EngineHost.shared.request(.status) { result in
            guard navigation.topViewController === expectedTop else { return }
            switch result {
            case .success(let state):
                guard let index = navigation.viewControllers.firstIndex(where: { $0 === controller }) else { return }
                var destination = Array(navigation.viewControllers.prefix(index))
                let profile = state["account_profile"] as? [String: Any] ?? [:]
                if state["enrolled"] as? Bool == true && profile["agreement_required"] as? Bool == true,
                    let account = (state["account"] as? [String: String])?["id"] {
                    destination.append(UserAgreementController(account: account, onContinue: onContinue))
                } else if profile["unlocked"] as? Bool == true && profile["display_name"] as? String == nil {
                    destination.append(AccountNameController(onlyIfMissing: true, onSaved: onContinue))
                } else { onContinue?() }
                navigation.setViewControllers(destination, animated: true)
            case .failure(let error):
                let alert = UIAlertController(title: "Couldn’t load account", message: error.localizedDescription, preferredStyle: .alert)
                alert.addAction(UIAlertAction(title: "Retry", style: .default) { _ in finish(from: controller, onContinue: onContinue) })
                alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
                controller.present(alert, animated: true)
            }
        }
    }
}


/// Read and accept the exact agreement bundled with the engine, including offline.
final class UserAgreementController: UIViewController {
    private let account: String?
    private let onContinue: (() -> Void)?
    private let accept = UIButton(type: .system)

    init(account: String? = nil, onContinue: (() -> Void)? = nil) {
        self.account = account
        self.onContinue = onContinue
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "User agreement"
        view.backgroundColor = .systemBackground
        let body = UITextView()
        body.isEditable = false
        body.accessibilityIdentifier = "user-agreement-document"
        body.adjustsFontForContentSizeCategory = true
        body.textContainerInset = UIEdgeInsets(top: 20, left: 16, bottom: 20, right: 16)
        let source = pb_ios_user_agreement()
        guard let bytes = source.bytes, (1...8192).contains(source.len),
              let markdown = String(bytes: UnsafeBufferPointer(start: bytes, count: source.len), encoding: .utf8) else {
            body.text = "The user agreement could not be opened."
            body.frame = view.bounds
            body.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            view.addSubview(body)
            return
        }
        let document = NSMutableAttributedString(string: "")
        for paragraph in markdown.trimmingCharacters(in: .whitespacesAndNewlines).components(separatedBy: "\n\n") {
            let heading = paragraph.hasPrefix("#")
            let text = heading ? String(paragraph.drop(while: { $0 == "#" || $0 == " " })) : paragraph
            let formatted = NSMutableAttributedString(attributedString:
                (try? AttributedString(markdown: text)).map { NSAttributedString($0) } ?? NSAttributedString(string: text))
            formatted.addAttributes([.font: UIFont.preferredFont(forTextStyle: heading ? .headline : .body),
                                     .foregroundColor: UIColor.label], range: NSRange(location: 0, length: formatted.length))
            if document.length > 0 { document.append(NSAttributedString(string: "\n\n")) }
            document.append(formatted)
        }
        body.attributedText = document
        accept.configuration = .filled()
        accept.setTitle("Agree and continue", for: .normal)
        accept.accessibilityIdentifier = "user-agreement-accept"
        accept.addTarget(self, action: #selector(acceptAgreement), for: .touchUpInside)
        view.addSubview(body)
        let readOnly = account == nil
        accept.isHidden = readOnly
        view.addSubview(accept)
        body.translatesAutoresizingMaskIntoConstraints = false
        accept.translatesAutoresizingMaskIntoConstraints = false
        let safe = view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            body.topAnchor.constraint(equalTo: safe.topAnchor), body.leadingAnchor.constraint(equalTo: safe.leadingAnchor),
            body.trailingAnchor.constraint(equalTo: safe.trailingAnchor), body.bottomAnchor.constraint(equalTo: readOnly ? safe.bottomAnchor : accept.topAnchor, constant: readOnly ? 0 : -12),
            accept.leadingAnchor.constraint(equalTo: safe.leadingAnchor, constant: 20),
            accept.trailingAnchor.constraint(equalTo: safe.trailingAnchor, constant: -20),
            accept.bottomAnchor.constraint(equalTo: safe.bottomAnchor, constant: -12),
            accept.heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
    }

    @objc private func acceptAgreement() {
        guard accept.isEnabled, let account else { return }
        accept.isEnabled = false
        EngineHost.shared.request(.acceptAgreement(account)) { result in
            self.accept.isEnabled = true
            switch result {
            case .success: AccountNameController.finish(from: self, onContinue: self.onContinue)
            case .failure(let error):
                let alert = UIAlertController(title: "Couldn’t save acceptance", message: error.localizedDescription, preferredStyle: .alert)
                alert.addAction(UIAlertAction(title: "OK", style: .default))
                self.present(alert, animated: true)
            }
        }
    }
}
