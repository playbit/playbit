import UIKit
import os

/// Entry only previews and retains an invitation. Joining and personal-volume
/// consent remain separate actions in InvitationController.
final class InvitationEntryController: UIViewController, UITextFieldDelegate, UIAdaptivePresentationControllerDelegate {
    private let field = UITextField()
    private let message = UILabel()
    private let open = UIButton(type: .system)
    private let onPreview: ([String: Any]) -> Void
    private var working = false
    private var canceled = false
    private static let performance = OSLog(subsystem: "app.playbit", category: "InvitationEntry")

    init(onPreview: @escaping ([String: Any]) -> Void) {
        self.onPreview = onPreview
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError() }

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Join Session"
        view.backgroundColor = .systemBackground
        if navigationController?.viewControllers.first === self {
            navigationItem.leftBarButtonItem = UIBarButtonItem(barButtonSystemItem: .cancel, target: self, action: #selector(cancel))
        } else {
            navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .cancel, target: self, action: #selector(cancel))
        }
        field.placeholder = "Session invitation"
        field.borderStyle = .roundedRect
        field.font = .preferredFont(forTextStyle: .body)
        field.adjustsFontForContentSizeCategory = true
        field.keyboardType = .URL
        field.autocorrectionType = .no
        field.autocapitalizationType = .none
        field.smartQuotesType = .no
        field.smartDashesType = .no
        field.returnKeyType = .go
        field.enablesReturnKeyAutomatically = true
        field.clearButtonMode = .whileEditing
        field.accessibilityIdentifier = "invitation-link"
        field.accessibilityLabel = "Session invitation"
        field.delegate = self
        field.addTarget(self, action: #selector(edited), for: .editingChanged)
        message.font = .preferredFont(forTextStyle: .body)
        message.adjustsFontForContentSizeCategory = true
        message.textColor = .systemRed
        message.numberOfLines = 0
        message.accessibilityIdentifier = "invitation-entry-error"
        message.isHidden = true
        open.configuration = .filled()
        open.setTitle("Open", for: .normal)
        open.accessibilityIdentifier = "invitation-open"
        open.addTarget(self, action: #selector(submit), for: .touchUpInside)
        let explanation = UILabel()
        explanation.text = "Paste the session invitation you received. You’ll review it before joining."
        explanation.font = .preferredFont(forTextStyle: .footnote)
        explanation.adjustsFontForContentSizeCategory = true
        explanation.textColor = .secondaryLabel
        explanation.numberOfLines = 0
        let stack = UIStackView(arrangedSubviews: [field, message, open, explanation])
        stack.axis = .vertical; stack.spacing = 20; stack.translatesAutoresizingMaskIntoConstraints = false
        let scroll = UIScrollView()
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.keyboardDismissMode = .interactive
        scroll.addSubview(stack); view.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 32),
            stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -32),
            stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 72),
            stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -24),
            stack.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor, constant: -64),
            field.heightAnchor.constraint(greaterThanOrEqualToConstant: 44),
            open.heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
        updateControls()
    }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        navigationController?.presentationController?.delegate = self
        if !canceled && !working { field.becomeFirstResponder() }
    }
    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isBeingDismissed || isMovingFromParent || navigationController?.isBeingDismissed == true { canceled = true }
    }
    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) { canceled = true }
    @objc private func cancel() {
        canceled = true
        dismiss(animated: true)
    }
    @objc private func edited() {
        message.text = nil; message.isHidden = true
        updateControls()
    }
    private func updateControls() {
        os_signpost(.begin, log: Self.performance, name: "Invitation controls")
        defer { os_signpost(.end, log: Self.performance, name: "Invitation controls") }
        field.isEnabled = !working
        open.isEnabled = !working && field.markedTextRange == nil && InvitationInput.canSubmit(field.text ?? "")
        open.setTitle(working ? "Checking invitation…" : "Open", for: .normal)
        open.configuration?.showsActivityIndicator = working
    }
    func textField(_ textField: UITextField, shouldChangeCharactersIn range: NSRange, replacementString string: String) -> Bool {
        os_signpost(.begin, log: Self.performance, name: "Invitation edit validation")
        defer { os_signpost(.end, log: Self.performance, name: "Invitation edit validation") }
        guard InvitationInput.acceptsChange(textField.text ?? "", range: range, replacement: string) else {
            message.text = "The invitation must be no larger than 32 KiB. Your previous text is unchanged."
            message.isHidden = false
            return false
        }
        return true
    }
    func textFieldShouldReturn(_ textField: UITextField) -> Bool { submit(); return false }
    func textFieldDidChangeSelection(_ textField: UITextField) {
        if !working { updateControls() }
    }
    @objc private func submit() {
        guard !working && !canceled && field.markedTextRange == nil,
              InvitationInput.canSubmit(field.text ?? "") else { return }
        guard let invitation = InvitationInput.submission(field.text ?? "") else {
            message.text = "Paste a session invitation."
            message.isHidden = false
            return
        }
        working = true
        message.text = nil; message.isHidden = true
        updateControls()
        EngineHost.shared.request(.previewInvitation(invitation)) { [weak self] result in
            guard let self, !self.canceled else { return }
            self.working = false
            self.updateControls()
            guard self.navigationController?.isBeingDismissed != true,
                  self.navigationController?.topViewController === self else { return }
            switch result {
            case .success(let preview): self.onPreview(preview)
            case .failure(let error):
                self.message.text = error.localizedDescription
                self.message.isHidden = false
            }
        }
    }
}
