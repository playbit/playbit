import UIKit
import AVFAudio
import os

final class GuestController: UIViewController, UITextFieldDelegate {
    // Ordinary navigation returns to its chosen owner. Explicit Quit releases it;
    // the Rust owner separately admits independent explicit Opens of one session.
    private static var selected: [String: GuestController] = [:]
    static func chosen(session: String) -> GuestController {
        if let current = selected[session] { return current }
        let controller = GuestController(session: session)
        selected[session] = controller
        return controller
    }
    private let session: String
    private var instance: String?
    private var generation: UInt64?
    private var openRequested = false
    private var quitting = false
    private var quitPending = false
    private let stack = UIStackView()
    private let scroll = UIScrollView()
    private var surface = GuestSurfaceDelivery()
    private let availability = UILabel()
    private let openingAction = UIButton(type: .system)
    private var opening = true
    private var openingPending = false
    private var openingRefreshNeeded = false
    private var runtimeError: String?
    private var openingPaused = false
    private let updateStack = UIStackView()
    private let updateMessage = UILabel()
    private let updateAction = UIButton(type: .system)
    private var pendingUpdate: [String: Any]?
    private var version = ""
    private var scene: GuestScene?
    private var guest: GuestNode?
    private var inputs = GuestInputQueue()
    private var inputPending = false
    // All controllers share the main-thread admission cap, including retained
    // sessions whose input responses are still in flight during navigation.
    private static var inputCount = 0
    private static var inputBytes = 0
    private static let performance = OSLog(subsystem: "app.playbit", category: "GuestInput")
    private final class GuestNode {
        var kind: GuestScene.Kind
        let id: UInt32
        let view: UIView
        var children: [GuestNode] = []
        init(kind: GuestScene.Kind, id: UInt32, view: UIView) { self.kind = kind; self.id = id; self.view = view }
    }
    private var invitation: String?
    private var timer: CADisplayLink?
    private var lastFrame: CFTimeInterval = 0
    private var framePending = false
    private var canvas: CanvasView?
    private var refreshPending = false
    private var refreshNeeded = false
    private var syncPending = false
    private static let audioSessionQueue = DispatchQueue(label: "app.playbit.audio-session", qos: .userInitiated)
    private var audioSessionReady = false
    private var audioActivationPending = false
    private var audioWanted = false
    private var audioEpoch: UInt64 = 0
    init(session: String) { self.session = session; super.init(nibName: nil, bundle: nil) }
    required init?(coder: NSCoder) { fatalError() }
    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Session"
        view.backgroundColor = .systemBackground
        navigationItem.rightBarButtonItem = UIBarButtonItem(title: "App", menu: appMenu())
        scroll.translatesAutoresizingMaskIntoConstraints = false; scroll.delaysContentTouches = false; scroll.canCancelContentTouches = false
        stack.axis = .vertical; stack.spacing = 12; stack.translatesAutoresizingMaskIntoConstraints = false
        availability.text = "Pending remote availability"
        availability.font = .preferredFont(forTextStyle: .footnote); availability.textColor = .secondaryLabel; availability.numberOfLines = 0
        stack.addArrangedSubview(availability)
        openingAction.configuration = .tinted()
        openingAction.setTitle("Cancel", for: .normal)
        openingAction.addTarget(self, action: #selector(openingActionPressed), for: .touchUpInside)
        stack.addArrangedSubview(openingAction)
        updateStack.axis = .vertical; updateStack.spacing = 8; updateStack.isHidden = true
        updateMessage.numberOfLines = 0; updateMessage.font = .preferredFont(forTextStyle: .footnote)
        updateAction.configuration = .tinted()
        updateAction.addTarget(self, action: #selector(reviewUpdate), for: .touchUpInside)
        updateStack.addArrangedSubview(updateMessage); updateStack.addArrangedSubview(updateAction)
        stack.addArrangedSubview(updateStack)
        scroll.addSubview(stack); view.addSubview(scroll)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor), scroll.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor), scroll.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor), scroll.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
            stack.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 16), stack.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -16), stack.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 16), stack.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -16), stack.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor, constant: -32)
        ])
        NotificationCenter.default.addObserver(self, selector: #selector(backgroundChanged(_:)), name: EngineHost.backgroundChanged, object: nil)
        startOpening()
        NotificationCenter.default.addObserver(self, selector: #selector(resume), name: UIApplication.didBecomeActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationResigned), name: UIApplication.willResignActiveNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(audioInterruption), name: AVAudioSession.interruptionNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(audioServicesReset), name: AVAudioSession.mediaServicesWereResetNotification, object: nil)
    }
    private func appMenu() -> UIMenu {
        UIMenu(children: [
            UIAction(title: "Copy invitation", image: UIImage(systemName: "link"), attributes: invitation == nil ? [.disabled] : []) { [weak self] _ in self?.copyInvitation() },
            UIAction(title: "Personal volumes", image: UIImage(systemName: "externaldrive")) { [weak self] _ in self?.personalVolumes() },
            UIAction(title: "Check for updates", image: UIImage(systemName: "arrow.clockwise")) { [weak self] _ in self?.checkUpdates() },
            UIAction(title: "Logs", image: UIImage(systemName: "text.alignleft")) { [weak self] _ in self?.logs() },
            UIAction(title: "Quit app", image: UIImage(systemName: "xmark.circle")) { [weak self] _ in self?.quit() }
        ])
    }
    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        notifySurface()
    }
    private func notifySurface() {
        guard !opening, !quitting,
            UIApplication.shared.applicationState == .active,
            navigationController?.topViewController === self,
            let window = viewIfLoaded?.window, let guest else { return }
        // The guest's stack position includes only host chrome above it. Content
        // scrolling and the guest's own height never change the viewport report.
        let width = max(0, scroll.bounds.width - 32)
        let height = max(0, scroll.bounds.height - 32 - guest.view.frame.minY)
        guard let value = GuestSurface(width: Double(width), height: Double(height),
            pixelScale: Float(window.screen.scale)) else {
            availability.text = "The app's display dimensions are unavailable."
            return
        }
        surface.notify(value) { self.enqueue([$0]) }
    }
    deinit {
        timer?.invalidate()
        NotificationCenter.default.removeObserver(self)
    }
    override func viewWillDisappear(_ animated: Bool) { super.viewWillDisappear(animated); suspend() }
    override func viewWillAppear(_ animated: Bool) { super.viewWillAppear(animated); resume() }
    @objc private func resume() {
        guard UIApplication.shared.applicationState == .active,
            navigationController?.topViewController === self else { return }
        audioWanted = true
        if audioSessionReady {
            pb_ios_audio_foreground(1)
            audioActive(true)
        } else {
            audioActive(false)
            prepareAudioSession()
        }
        if opening { if !openingPaused { checkOpening() }; return }
        view.setNeedsLayout()
        startFrames()
        refreshScene()
    }
    @objc private func suspend() {
        cancelAudioActivation()
        audioActive(false)
        timer?.invalidate(); timer = nil; lastFrame = 0
    }
    private func cancelAudioActivation() {
        audioWanted = false; audioSessionReady = false
        audioEpoch &+= 1
    }
    private func prepareAudioSession() {
        guard !audioActivationPending, audioWanted, !quitting,
            UIApplication.shared.applicationState == .active,
            navigationController?.topViewController === self else { return }
        audioActivationPending = true
        let epoch = audioEpoch
        // One operation per controller is admitted at a time. A later resume
        // coalesces behind it; suspension invalidates its main-thread completion.
        Self.audioSessionQueue.async { [weak self] in
            let failure: String?
            do {
                try AVAudioSession.sharedInstance().setCategory(.ambient, mode: .default)
                try AVAudioSession.sharedInstance().setActive(true)
                failure = nil
            } catch { failure = error.localizedDescription }
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.audioActivationPending = false
                guard epoch == self.audioEpoch else {
                    if self.audioWanted { self.prepareAudioSession() }
                    return
                }
                guard self.audioWanted, !self.quitting,
                    UIApplication.shared.applicationState == .active,
                    self.navigationController?.topViewController === self else { return }
                if let failure {
                    pb_ios_audio_foreground(0)
                    self.audioActive(false)
                    self.availability.text = "Sound is unavailable: \(failure)"
                } else {
                    self.audioSessionReady = true
                    pb_ios_audio_foreground(1)
                    self.audioActive(true)
                }
            }
        }
    }
    @objc private func applicationResigned() {
        // willResignActive can precede the applicationState transition.
        pb_ios_audio_foreground(0)
        suspend()
    }
    @objc private func audioServicesReset() {
        guard UIApplication.shared.applicationState == .active,
            navigationController?.topViewController === self else { return }
        // Reconfigure the replacement service and advance the output retry epoch.
        pb_ios_audio_foreground(0)
        cancelAudioActivation()
        audioActive(false)
        resume()
    }
    private func audioActive(_ active: Bool) {
        guard let instance else { return }
        let result = Array(instance.utf8).withUnsafeBufferPointer {
            pb_ios_audio_active($0.baseAddress, $0.count, active ? 1 : 0)
        }
        if result != 0 { availability.text = "The app's sound identity is unavailable." }
    }
    @objc private func audioInterruption(_ notification: Notification) {
        guard let raw = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
              let kind = AVAudioSession.InterruptionType(rawValue: raw) else { return }
        if kind == .began {
            pb_ios_audio_foreground(0)
            cancelAudioActivation()
            audioActive(false)
        }
        else if UIApplication.shared.applicationState == .active,
                navigationController?.topViewController === self { resume() }
    }
    @objc private func backgroundChanged(_ notification: Notification) {
        if let error = notification.userInfo?["error"] as? String {
            runtimeError = error; availability.text = error
        }
        if opening { checkOpening() }
        else if syncPending { checkSync() }
        else { refreshScene() }
    }
    private func refreshScene() {
        guard let instance, !quitting, !openingPaused, UIApplication.shared.applicationState == .active,
            navigationController?.topViewController === self else { return }
        guard !refreshPending else { refreshNeeded = true; return }
        refreshPending = true
        EngineHost.shared.request(.appFrame(instance)) { result in
            self.refreshPending = false
            if self.opening, case .failure(let error) = result { self.openingFailed(error.localizedDescription) }
            else { self.receive(result) }
            if self.refreshNeeded {
                self.refreshNeeded = false
                self.refreshScene()
            }
        }
    }
    private func startOpening() {
        guard !openingPending else { return }
        runtimeError = nil
        opening = true; openingPaused = false
        surface.reset()
        openingAction.isHidden = false; openingAction.isEnabled = true
        openingAction.setTitle("Cancel", for: .normal)
        availability.text = "Opening your app and restoring shared data…"
        if instance != nil { refreshScene(); return }
        // Initial appearance and an explicit Resume action may prepare an app.
        // Completion notices only inspect this attempt; they never replay Open.
        openRequested = false
        openingPending = true
        EngineHost.shared.request(.prepareSession(session)) { result in
            self.openingPending = false
            switch result {
            case .failure(let error): self.openingFailed(error.localizedDescription)
            case .success(let value):
                if value["prepared_session"] as? String == self.session { self.openPreparedGuest() }
                else { self.checkOpening() }
            }
        }
    }
    private func openPreparedGuest() {
        guard instance == nil, !openRequested, !openingPending, !openingPaused else { return }
        openRequested = true; openingPending = true
        openingAction.isEnabled = false
        EngineHost.shared.request(.openApp(session)) { result in
            self.openingPending = false
            switch result {
            case .failure(let error): self.openingFailed(error.localizedDescription)
            case .success(let value):
                guard let instance = value["instance_id"] as? String,
                    let generation = value["generation"] as? UInt64, generation > 0 else {
                    self.openingFailed("The app's opening result is unavailable.")
                    return
                }
                self.instance = instance; self.generation = generation
                self.audioActive(self.audioSessionReady && self.audioWanted
                    && UIApplication.shared.applicationState == .active
                    && self.navigationController?.topViewController === self)
                Self.selected[self.session] = self
                self.openingAction.isEnabled = true
                self.openingRefreshNeeded = false
                self.refreshScene()
            }
        }
    }
    private func openingFailed(_ message: String) {
        if instance == nil, Self.selected[session] === self { Self.selected[session] = nil }
        openingPaused = true
        availability.text = "\(message) Your app and saved data are retained."
        openingAction.isEnabled = true; openingAction.setTitle("Resume opening", for: .normal)
    }
    @objc private func openingActionPressed() {
        if openingPaused { startOpening(); return }
        openingAction.isEnabled = false
        if let instance {
            EngineHost.shared.request(.quitApp(instance)) { result in
                switch result {
                case .failure(let error): self.openingFailed(error.localizedDescription)
                case .success(let value):
                    guard value["quit"] as? String == instance else { self.openingFailed("The app's Quit result is unavailable."); return }
                    self.closedInstance()
                    self.openingFailed(value["drain_failed"] as? Bool == true
                        ? "The app closed before finishing its work. Committed data is retained; unfinished work will not be repeated."
                        : "Opening paused.")
                }
            }
            return
        }
        EngineHost.shared.request(.cancelTransfer) { result in
            if case .failure(let error) = result { self.openingFailed(error.localizedDescription) }
            else { self.checkOpening() }
        }
    }
    private func checkOpening() {
        guard opening, !openingPaused,
            UIApplication.shared.applicationState == .active,
            navigationController?.topViewController === self else { return }
        guard !openingPending else { openingRefreshNeeded = true; return }
        if instance != nil { refreshScene(); return }
        guard !openRequested else { return }
        openingPending = true
        EngineHost.shared.request(.transferStatus) { result in
            self.openingPending = false
            let refresh = self.openingRefreshNeeded
            self.openingRefreshNeeded = false
            defer { if refresh { self.checkOpening() } }
            switch result {
            case .failure(let error): self.openingFailed(error.localizedDescription)
            case .success(let value):
                let transfer = value["transfer"] as? [String: Any] ?? [:]
                switch transfer["kind"] as? String {
                case "complete": self.openPreparedGuest()
                case "failed":
                    self.openingFailed(transfer["message"] as? String ?? "The app could not open yet.")
                    self.pendingUpdate = transfer["update"] as? [String: Any]
                    self.updateStack.isHidden = self.pendingUpdate == nil
                    if let update = self.pendingUpdate {
                        self.updateMessage.text = update["message"] as? String
                        self.updateAction.setTitle("Retry update", for: .normal)
                    }
                case "paused": self.openingFailed("Opening paused.")
                default:
                    self.availability.text = transfer["message"] as? String ?? "Restoring shared data…"
                    // Completion arrives through EngineHost.backgroundChanged.
                }
            }
        }
    }
    private func requestUpdateCheck() {
        runtimeError = nil
        guard !opening, !syncPending, UIApplication.shared.applicationState == .active else { return }
        syncPending = true
        EngineHost.shared.request(.status) { result in
            guard case .success(let status) = result, status["enrolled"] as? Bool == true else {
                self.syncPending = false
                return
            }
            EngineHost.shared.request(.checkUpdates(self.session)) { result in
                switch result {
                case .failure(let error):
                    self.syncPending = false
                    self.availability.text = "Saved on this device. \(error.localizedDescription)"
                case .success: self.checkSync()
                }
            }
        }
    }
    private func checkSync() {
        guard UIApplication.shared.applicationState == .active, navigationController?.topViewController === self else { syncPending = false; return }
        EngineHost.shared.request(.transferStatus) { result in
            switch result {
            case .failure(let error): self.syncPending = false; self.availability.text = error.localizedDescription
            case .success(let value):
                let transfer = value["transfer"] as? [String: Any] ?? [:]
                if transfer["kind"] as? String == "running" || transfer["kind"] as? String == "canceling" {
                    // Completion arrives through EngineHost.backgroundChanged.
                } else {
                    self.syncPending = false
                    if transfer["kind"] as? String == "failed" { self.availability.text = transfer["message"] as? String ?? "Waiting to synchronize" }
                    else { self.refreshScene() }
                }
            }
        }
    }
    private func receive(_ result: Result<[String: Any], Error>, acknowledgment: GuestInput? = nil) {
        os_signpost(.begin, log: Self.performance, name: "Scene apply")
        defer { os_signpost(.end, log: Self.performance, name: "Scene apply") }
        switch result {
        case .failure(let error):
            availability.text = error.localizedDescription
            timer?.invalidate(); timer = nil
        case .success(let value):
            guard value["instance_id"] as? String == instance,
                let generation = value["generation"] as? UInt64, generation > 0 else {
                availability.text = "The app's frame belongs to a different session instance."
                suspend(); return
            }
            self.generation = generation
            #if PHASE1_QUALIFICATION
            // Exercise the actual adoption/error path after a real guest scene.
            // Ordinary builds contain neither the injection nor its option.
            if scene != nil, ProcessInfo.processInfo.arguments.contains("--phase1-malformed-scene") {
                if pb_ios_qualification_malformed_scene() == 0 {
                    availability.text = "The app scene is invalid. The previous view is retained."
                    timer?.invalidate(); timer = nil
                    return
                }
            }
            #endif
            let nextInvitation = (value["invitation"] as? String).flatMap { $0.isEmpty ? nil : $0 }
            if invitation != nextInvitation {
                invitation = nextInvitation
                navigationItem.rightBarButtonItem?.menu = appMenu()
            }
            let nextVersion = (value["session"] as? [String: Any])?["resolved_build_id"] as? String ?? version
            if nextVersion != version { surface.reset() }
            version = nextVersion
            pendingUpdate = value["update"] as? [String: Any]
            updateStack.isHidden = pendingUpdate == nil
            if let update = pendingUpdate {
                updateMessage.text = update["message"] as? String ?? "An app update needs your attention."
                updateAction.setTitle("Retry update", for: .normal)
            }
            if !quitting { availability.text = runtimeError ?? (value["availability"] as? String ?? "Pending remote availability") }
            guard let next = value["scene"] as? GuestScene else { return }
            if opening { opening = false; openingAction.isHidden = true }
            if next != scene {
                scene = next
                let previous = guest
                guest = reconcile(next, previous: previous, acknowledgment: acknowledgment)
                if previous?.view !== guest?.view {
                    if let old = previous?.view { stack.removeArrangedSubview(old); old.removeFromSuperview() }
                    if let current = guest?.view { stack.addArrangedSubview(current) }
                }
                canvas = findCanvasView(guest)

            }
            view.setNeedsLayout()
            startFrames()
        }
    }
    private func findCanvasView(_ node: GuestNode?) -> CanvasView? {
        guard let node else { return nil }
        if node.id != 0, let view = node.view as? CanvasView,
           view.scene?.frames == 1 { return view }
        return node.children.lazy.compactMap { self.findCanvasView($0) }.first
    }
    private func field(_ id: UInt32, in node: GuestNode?) -> GuestTextField? {
        guard let node else { return nil }
        if node.id == id, let field = node.view as? GuestTextField { return field }
        return node.children.lazy.compactMap { self.field(id, in: $0) }.first
    }
    private func reconcile(_ node: GuestScene, previous: GuestNode?, acknowledgment: GuestInput?) -> GuestNode {
        let kind = node.kind, id = node.id
        let result: GuestNode
        let isContainer = kind == .row || kind == .column
        if let previous, previous.id == id,
            previous.kind == kind || (isContainer && previous.view is UIStackView) {
            result = previous
            result.kind = kind
        }
        else {
            let view: UIView
            switch kind {
            case .column, .row:
                let container = UIStackView(); container.axis = kind == .row ? .horizontal : .vertical; container.spacing = 8
                container.alignment = kind == .row ? .center : .fill
                view = container
            case .text:
                let label = UILabel(); label.numberOfLines = 0; label.adjustsFontForContentSizeCategory = true; view = label
            case .textField:
                let field = GuestTextField(); field.tag = Int(id); field.delegate = self; field.returnKeyType = .done
                field.accessibilityIdentifier = "guest-input-\(id)"
                field.addTarget(self, action: #selector(textChanged(_:)), for: .editingChanged)
                view = field
            case .button, .checkbox:
                let control = GuestControl(); control.tag = Int(id); control.accessibilityIdentifier = "guest-control-\(id)"
                control.addTarget(self, action: #selector(activated(_:)), for: .touchUpInside); view = control
            case .canvas:
                let canvas = CanvasView()
                if id != 0 {
                    canvas.event = { [weak self] words in self?.send(kind: 11, target: id, data: words.flatMap(Self.bytes)) }
                }
                canvas.isUserInteractionEnabled = id != 0
                if id != 0 {
                    canvas.accessibilityIdentifier = "guest-canvas"; canvas.isAccessibilityElement = true; canvas.accessibilityLabel = "Interactive game canvas"
                }
                view = canvas
            }
            result = GuestNode(kind: kind, id: id, view: view)
        }
        if let container = result.view as? UIStackView {
            container.axis = kind == .row ? .horizontal : .vertical
            container.alignment = kind == .row ? .center : .fill
            let nodes = node.children
            let old = result.children
            result.children = nodes.enumerated().map { index, child in
                let childID = child.id
                let prior = childID == 0 ? (old.indices.contains(index) ? old[index] : nil) : old.first { $0.id == childID }
                return reconcile(child, previous: prior, acknowledgment: acknowledgment)
            }
            for prior in old where !result.children.contains(where: { $0 === prior }) {
                container.removeArrangedSubview(prior.view); prior.view.removeFromSuperview()
            }
            for (index, child) in result.children.enumerated() {
                if container.arrangedSubviews.count <= index || container.arrangedSubviews[index] !== child.view {
                    container.insertArrangedSubview(child.view, at: index)
                }
            }
        } else if let field = result.view as? GuestTextField {
            field.placeholder = node.placeholder; field.isEnabled = !node.disabled
            field.readOnly = node.readOnly
            field.accessibilityLabel = field.placeholder
            field.apply(value: node.text, acknowledgment: acknowledgment?.target == id ? acknowledgment?.revision : nil)
        } else if let control = result.view as? GuestControl { control.apply(node) }
        else if let label = result.view as? UILabel, kind == .text {
            label.text = node.text
            switch node.style {
            case 1: label.font = .preferredFont(forTextStyle: .title1)
            case 2: label.font = .preferredFont(forTextStyle: .footnote)
            case 3: label.font = .preferredFont(forTextStyle: .caption1)
            case 4: label.font = .preferredFont(forTextStyle: .headline)
            case 5: label.font = .preferredFont(forTextStyle: .title3)
            default: label.font = .preferredFont(forTextStyle: .body)
            }
            label.textColor = node.style == 2 ? .secondaryLabel : .label
        } else if let canvas = result.view as? CanvasView { canvas.scene = node; canvas.setNeedsDisplay() }
        return result
    }
    @objc private func textChanged(_ field: GuestTextField) {
        guard !field.readOnly else { return }
        field.revision.edit()
        let text = field.text ?? ""
        field.edits.didChange(text, composing: field.markedTextRange != nil)
        guard field.markedTextRange == nil else { return }
        if enqueue([GuestInput(kind: 14, target: UInt32(field.tag), data: field.edits.payload(text), revision: field.revision.value)]) {
            field.edits.queued()
        }
    }
    func textField(_ textField: UITextField, shouldChangeCharactersIn range: NSRange, replacementString string: String) -> Bool {
        guard let field = textField as? GuestTextField, !field.readOnly,
            let current = field.text, let span = Range(range, in: current) else { return false }
        let permitted = current.replacingCharacters(in: span, with: string).utf8.count <= GuestTextEdits.textBytesMax
            && field.edits.willChange(current, range: range, replacement: string)
        if !permitted { availability.text = "Text is limited to 256 KiB. Your draft is retained." }
        return permitted
    }
    func textFieldShouldReturn(_ textField: UITextField) -> Bool {
        guard let field = textField as? GuestTextField, field.isEnabled, !field.readOnly, field.markedTextRange == nil else { return false }
        if enqueue([GuestInput(kind: 3, target: UInt32(field.tag), data: Data((field.text ?? "").utf8), revision: field.revision.value)]) {
            field.edits.queued()
        }
        return false
    }
    private func editableFields(in node: GuestNode?) -> [GuestTextField] {
        guard let node else { return [] }
        if let field = node.view as? GuestTextField { return [field] }
        return node.children.flatMap { editableFields(in: $0) }
    }
    @objc private func activated(_ control: GuestControl) {
        guard control.isEnabled else { return }
        var requests: [GuestInput] = []
        var bytes = 128
        for field in editableFields(in: guest) where field.isEnabled && !field.readOnly && field.revision.dirty {
            // Do not commit another control action against unfinished IME text.
            guard field.markedTextRange == nil else { return }
            let text = field.text ?? ""
            let data = field.edits.payload(text)
            bytes += data.count + 128
            guard requests.count + Self.inputCount + 2 <= 32, bytes + Self.inputBytes <= 2 * 1024 * 1024 else {
                availability.text = "The app is busy. Your draft is retained; try again."; return
            }
            requests.append(GuestInput(kind: 14, target: UInt32(field.tag), data: data, revision: field.revision.value, requiresPreviousSuccess: !requests.isEmpty))
        }
        requests.append(GuestInput(kind: 2, target: UInt32(control.tag), data: Data(), revision: 0, requiresPreviousSuccess: !requests.isEmpty))
        if enqueue(requests) {
            for field in editableFields(in: guest) where requests.contains(where: { $0.kind == 14 && $0.target == UInt32(field.tag) }) {
                field.edits.queued()
            }
        }
    }
    private func send(kind: Int, target: UInt32, data: [UInt8] = [], revision: UInt64 = 0) {
        enqueue([GuestInput(kind: kind, target: target, data: Data(data), revision: revision)])
    }
    @discardableResult private func enqueue(_ requests: [GuestInput]) -> Bool {
        let bytes = requests.reduce(0) { $0 + $1.bytes }
        guard !quitting, Self.inputCount + requests.count <= 32, Self.inputBytes + bytes <= 2 * 1024 * 1024,
            inputs.admit(requests) else { availability.text = "The app is busy. Your draft is retained; try again."; return false }
        Self.inputCount += requests.count; Self.inputBytes += bytes
        dispatchInput(); return true
    }
    private func dispatchInput() {
        guard !inputPending, let instance, let input = inputs.first else { return }
        inputPending = true
        os_signpost(.begin, log: Self.performance, name: "Local guest input")
        EngineHost.shared.input(instance: instance, kind: input.kind, target: input.target, data: input.data) { result in
            os_signpost(.end, log: Self.performance, name: "Local guest input")
            if case .success = result, input.revision != 0 {
                self.field(input.target, in: self.guest)?.revision.acknowledge(input.revision)
            }
            self.receive(result, acknowledgment: input)
            let count = self.inputs.count, bytes = self.inputs.bytes
            if case .success = result { self.inputs.finish(success: true) } else { self.inputs.finish(success: false) }
            Self.inputCount -= count - self.inputs.count; Self.inputBytes -= bytes - self.inputs.bytes
            if input.kind == 13 { self.framePending = false }
            self.inputPending = false; self.dispatchInput(); self.finishQuit()
            self.view.setNeedsLayout() // Queue progress retries unadmitted geometry.
        }
    }
    private static func bytes(_ word: UInt32) -> [UInt8] { [UInt8(truncatingIfNeeded: word), UInt8(truncatingIfNeeded: word >> 8), UInt8(truncatingIfNeeded: word >> 16), UInt8(truncatingIfNeeded: word >> 24)] }
    private func startFrames() {
        guard !opening, !quitting, canvas != nil, timer == nil, viewIfLoaded?.window != nil, UIApplication.shared.applicationState == .active else { return }
        let display = CADisplayLink(target: self, selector: #selector(frame(_:))); display.preferredFrameRateRange = CAFrameRateRange(minimum: 15, maximum: 30, preferred: 30); display.add(to: .main, forMode: .common); timer = display
    }
    @objc private func frame(_ display: CADisplayLink) {
        let elapsed = lastFrame == 0 ? 1.0 / 30.0 : min(0.1, max(0.000001, display.timestamp - lastFrame)); lastFrame = display.timestamp
        guard !opening, !framePending, inputs.count == 0, let id = canvas?.scene?.id else { return }
        framePending = enqueue([GuestInput(kind: 13, target: id, data: Data(Self.bytes(UInt32(elapsed * 1_000_000))), revision: 0)])
    }
    @objc private func copyInvitation() {
        guard let invitation else { return }
        UIPasteboard.general.string = invitation
        availability.text = "Invitation copied"
    }
    private func personalVolumes() {
        guard !version.isEmpty else { return }
        navigationController?.pushViewController(PersonalVolumesController(session: session), animated: true)
    }
    @objc private func reviewUpdate() { checkUpdates() }
    private func checkUpdates() {
        if opening { startOpening() } else { requestUpdateCheck() }
    }
    private func quit() {
        guard instance != nil, !quitting else { return }
        quitting = true; suspend()
        scroll.isUserInteractionEnabled = false
        navigationItem.rightBarButtonItem?.isEnabled = false
        availability.text = "Finishing accepted work…"
        finishQuit()
    }
    private func finishQuit() {
        guard quitting, !quitPending, !inputPending, inputs.first == nil, let instance else { return }
        quitPending = true
        EngineHost.shared.request(.quitApp(instance)) { result in
            self.quitPending = false
            switch result {
            case .failure(let error):
                self.quitting = false
                self.scroll.isUserInteractionEnabled = true
                self.navigationItem.rightBarButtonItem?.isEnabled = true
                self.availability.text = error.localizedDescription
            case .success(let value):
                guard value["quit"] as? String == instance else {
                    self.availability.text = "The app's Quit result is unavailable."
                    return
                }
                if Self.selected[self.session] === self { Self.selected[self.session] = nil }
                self.closedInstance()
                self.availability.text = "App closed."
                if value["drain_failed"] as? Bool == true {
                    let alert = UIAlertController(title: "App closed before finishing its work",
                        message: "Saved data is retained. Some actions may be unfinished; Playbit will not repeat them.", preferredStyle: .alert)
                    alert.addAction(UIAlertAction(title: "OK", style: .default) { _ in
                        self.navigationController?.popViewController(animated: true)
                    })
                    self.present(alert, animated: true)
                } else {
                    self.navigationController?.popViewController(animated: true)
                }
            }
        }
    }
    static var canSwitchEnvironment: Bool {
        inputCount == 0 && selected.values.allSatisfy { controller in
            controller.editableFields(in: controller.guest).allSatisfy { !$0.revision.dirty }
        }
    }
    static func retireAccount() {
        for controller in selected.values { controller.closedInstance() }
        selected.removeAll()
    }
    private func closedInstance() {
        instance = nil; generation = nil
        Self.inputCount -= inputs.count; Self.inputBytes -= inputs.bytes
        inputs = GuestInputQueue(); framePending = false
        if let view = guest?.view { stack.removeArrangedSubview(view); view.removeFromSuperview() }
        guest = nil; scene = nil; canvas = nil; version = ""
        suspend(); surface.reset()
    }
    @objc private func logs() {
        guard let instance else { return }
        navigationController?.pushViewController(LogsController(instance: instance), animated: true)
    }
}

final class CanvasView: UIView {
    private var aspect: NSLayoutConstraint?
    var scene: GuestScene? {
        didSet {
            guard oldValue?.width != scene?.width || oldValue?.height != scene?.height else { return }
            aspect?.isActive = false
            aspect = nil
            if let scene, scene.width > 0, scene.height > 0 {
                aspect = heightAnchor.constraint(equalTo: widthAnchor,
                    multiplier: CGFloat(scene.height) / CGFloat(scene.width))
                aspect?.isActive = true
            }
        }
    }
    var event: (([UInt32]) -> Void)?
    override func draw(_ rect: CGRect) {
        guard let context = UIGraphicsGetCurrentContext(), let scene,
              scene.width > 0, scene.height > 0 else { return }
        context.scaleBy(x: bounds.width / CGFloat(scene.width), y: bounds.height / CGFloat(scene.height))
        for item in scene.items {
            let geometry: GuestScene.Rect
            switch item {
            case .rect(let value), .text(let value, _, _, _), .pixels(let value, _, _, _): geometry = value
            }
            let rgba = geometry.color
            let color = UIColor(displayP3Red: CGFloat((rgba >> 24) & 255)/255, green: CGFloat((rgba >> 16) & 255)/255, blue: CGFloat((rgba >> 8) & 255)/255, alpha: CGFloat(rgba & 255)/255)
            let area = CGRect(x: CGFloat(geometry.x), y: CGFloat(geometry.y), width: CGFloat(geometry.width), height: CGFloat(geometry.height))
            if !area.intersects(context.boundingBoxOfClipPath) { continue }
            switch item {
            case .rect: context.setFillColor(color.cgColor); context.fill(area)
            case .text(_, let fontSize, let bold, let text):
                context.saveGState(); context.clip(to: area)
                (text as NSString).draw(at: area.origin, withAttributes: [.font: UIFont.systemFont(ofSize: CGFloat(fontSize), weight: bold ? .bold : .regular), .foregroundColor: color])
                context.restoreGState()
            case .pixels(_, let width, let height, let rgba):
                guard let provider = CGDataProvider(data: rgba as CFData),
                      let space = CGColorSpace(name: CGColorSpace.sRGB),
                      let image = CGImage(width: Int(width), height: Int(height), bitsPerComponent: 8,
                        bitsPerPixel: 32, bytesPerRow: Int(width) * 4, space: space,
                        bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                        provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent) else { continue }
                context.saveGState()
                context.interpolationQuality = .none
                context.translateBy(x: area.minX, y: area.maxY)
                context.scaleBy(x: 1, y: -1)
                context.draw(image, in: CGRect(origin: .zero, size: area.size))
                context.restoreGState()
            }
        }
    }
    private func touch(_ touches: Set<UITouch>, phase: UInt32) {
        guard let point = touches.first?.location(in: self), point.x.isFinite, point.y.isFinite,
              bounds.width.isFinite, bounds.height.isFinite, bounds.width > 0, bounds.height > 0,
              let scene, scene.width > 0, scene.height > 0 else { return }
        let width = CGFloat(scene.width), height = CGFloat(scene.height)
        event?([UInt32(max(0, min(width, point.x / bounds.width * width))), UInt32(max(0, min(height, point.y / bounds.height * height))), phase, phase == 2 || phase == 3 ? 0 : 1])
    }
    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) { touch(touches, phase: 1) }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) { touch(touches, phase: 0) }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { touch(touches, phase: 2) }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { touch(touches, phase: 3) }
}
