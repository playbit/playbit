import UIKit
import PushKit

@main
final class AppDelegate: UIResponder, UIApplicationDelegate, PKPushRegistryDelegate {
    private var providerPush: PKPushRegistry?
    private var providerToken: Data?
    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?) -> Bool {
        let registry = PKPushRegistry(queue: .main)
        registry.delegate = self
        providerPush = registry
        registry.desiredPushTypes = [.fileProvider]
        if let token = registry.pushToken(for: .fileProvider), !token.isEmpty, token.count <= 128 {
            providerToken = token
            EngineHost.shared.providerTokenChanged(token)
        }
        return true
    }
    func pushRegistry(_ registry: PKPushRegistry, didUpdate credentials: PKPushCredentials, for type: PKPushType) {
        guard type == .fileProvider, !credentials.token.isEmpty, credentials.token.count <= 128 else { return }
        providerToken = credentials.token
        EngineHost.shared.providerTokenChanged(credentials.token)
    }
    func pushRegistry(_ registry: PKPushRegistry, didInvalidatePushTokenFor type: PKPushType) {
        guard type == .fileProvider, let token = providerToken else { return }
        EngineHost.shared.providerTokenChanged(token, invalidated: true)
        providerToken = nil
    }
}

/// Native chrome keeps the selected account authority visible throughout navigation.
final class EnvironmentNavigationController: UINavigationController, UINavigationControllerDelegate {
    override func viewDidLoad() {
        super.viewDidLoad()
        delegate = self
    }
    func navigationController(_ navigationController: UINavigationController,
                              willShow viewController: UIViewController, animated: Bool) {
        let testing = EngineHost.shared.channel == .testing
        if testing {
            let label = UILabel()
            label.text = "Testing"
            label.font = .preferredFont(forTextStyle: .footnote)
            label.adjustsFontForContentSizeCategory = true
            label.textColor = .secondaryLabel
            label.accessibilityIdentifier = "testing-channel"
            viewController.toolbarItems = [.flexibleSpace(), UIBarButtonItem(customView: label), .flexibleSpace()]
        }
        setToolbarHidden(!testing, animated: false)
    }
}

final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?
    func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
        guard let scene = scene as? UIWindowScene else { return }
        let window = UIWindow(windowScene: scene)
        #if PHASE1_QUALIFICATION
        if ProcessInfo.processInfo.arguments.contains("--phase1-corrupt-store") {
            let profile = EngineHost.shared.profile
            precondition(!FileManager.default.fileExists(atPath: profile.path), "Corrupt-store fixture requires a fresh app container")
            do {
                try FileManager.default.createDirectory(at: profile, withIntermediateDirectories: true)
                try Data("damaged database".utf8).write(to: profile.appendingPathComponent("profile.kvdb"))
            } catch { preconditionFailure("Could not prepare corrupt-store fixture: \(error)") }
        }
        #endif
        let marker = EngineHost.shared.profile.appendingPathComponent("control/engine-crash")
        if EngineHost.shared.channel != nil && FileManager.default.fileExists(atPath: marker.path) {
            // This process is fresh. Never reopen a crashed engine in its old process.
            let recovery = UIViewController()
            recovery.view.backgroundColor = .systemBackground
            let message = UILabel()
            message.text = "Playbit stopped unexpectedly. Restart Playbit to continue. The last action may have completed and will not be repeated automatically."
            message.numberOfLines = 0
            message.font = .preferredFont(forTextStyle: .body)
            message.adjustsFontForContentSizeCategory = true
            let restart = UIButton(type: .system)
            restart.setTitle("Restart Playbit", for: .normal)
            restart.addAction(UIAction { [weak window] _ in
                do {
                    try FileManager.default.removeItem(at: marker)
                    window?.rootViewController = EnvironmentNavigationController(rootViewController: SessionsController())
                } catch { message.text = "Playbit could not restart: \(error.localizedDescription). Close and reopen Playbit to try again." }
            }, for: .touchUpInside)
            let content = UIStackView(arrangedSubviews: [message, restart])
            content.axis = .vertical; content.spacing = 24
            content.translatesAutoresizingMaskIntoConstraints = false
            recovery.view.addSubview(content)
            NSLayoutConstraint.activate([
                content.leadingAnchor.constraint(equalTo: recovery.view.safeAreaLayoutGuide.leadingAnchor, constant: 24),
                content.trailingAnchor.constraint(equalTo: recovery.view.safeAreaLayoutGuide.trailingAnchor, constant: -24),
                content.centerYAnchor.constraint(equalTo: recovery.view.centerYAnchor)
            ])
            window.rootViewController = recovery
        } else {
            let navigation = EnvironmentNavigationController(rootViewController: SessionsController())
            if options.urlContexts.first?.url.isFileURL == true {
                navigation.pushViewController(VolumeBrowserController(), animated: false)
            }
            window.rootViewController = navigation
        }
        window.makeKeyAndVisible()
        self.window = window
        #if PHASE1_QUALIFICATION
        if ProcessInfo.processInfo.arguments.contains("--phase1-account-profile") {
            (window.rootViewController as? UINavigationController)?.pushViewController(AccountNameController(), animated: false)
        }
        if ProcessInfo.processInfo.arguments.contains("--phase1-account-settings") {
            EngineHost.shared.request(.status) { [weak window] result in
                if case .success(let state) = result {
                    (window?.rootViewController as? UINavigationController)?.pushViewController(AccountController(state: state), animated: false)
                }
            }
        }
        #endif
        if let url = options.urlContexts.first?.url { openInvitation(url) }
    }
    func scene(_ scene: UIScene, openURLContexts contexts: Set<UIOpenURLContext>) {
        if let url = contexts.first?.url { openInvitation(url) }
    }
    private func openInvitation(_ url: URL) {
        if url.isFileURL, let navigation = window?.rootViewController as? UINavigationController {
            DatabaseDocuments.open(url, navigation: navigation)
            return
        }
        DatabaseDocuments.cancelPendingOpen()
        // playbit:session:CHANNEL:CODE carries the invitation's environment;
        // the runtime validates it before opening or retaining the invitation.
        guard url.scheme == "playbit",
              let navigation = window?.rootViewController as? UINavigationController,
              let sessions = navigation.viewControllers.first as? SessionsController else { return }
        // Safari can deliver a foregrounding URL before the scene's foreground
        // callback. Serialize resume before the invitation's runtime request.
        EngineHost.shared.resume()
        if navigation.presentedViewController != nil {
            navigation.dismiss(animated: false) { sessions.receiveInvitation(url.absoluteString) }
        } else {
            sessions.receiveInvitation(url.absoluteString)
        }
    }
    func sceneWillEnterForeground(_ scene: UIScene) { EngineHost.shared.resume() }
    func sceneDidEnterBackground(_ scene: UIScene) {
        let application = UIApplication.shared
        // Allow the serialized worker to pause a join and close its account
        // connection before iOS suspends execution. Saved data stays in place.
        var task = UIBackgroundTaskIdentifier.invalid
        let finish = {
            guard task != .invalid else { return }
            application.endBackgroundTask(task)
            task = .invalid
        }
        task = application.beginBackgroundTask(withName: "Pause Playbit transfers", expirationHandler: finish)
        EngineHost.shared.suspend(completion: finish)
    }
}

final class CorruptStoreController: UIViewController {
    private let error = UILabel()
    private let reset = UIButton(type: .system)

    override func viewDidLoad() {
        super.viewDidLoad()
        title = "Playbit"
        view.backgroundColor = .systemBackground
        let heading = UILabel()
        heading.text = "Oh no…"
        heading.font = .preferredFont(forTextStyle: .title2)
        heading.textColor = .secondaryLabel
        heading.adjustsFontForContentSizeCategory = true
        let detail = UILabel()
        detail.text = "The local store has become corrupt. Some data may not have been backed up."
        detail.font = .preferredFont(forTextStyle: .body)
        detail.textColor = .secondaryLabel
        detail.numberOfLines = 0
        detail.adjustsFontForContentSizeCategory = true
        var button = UIButton.Configuration.filled()
        button.title = "Reset & Sign in"
        button.cornerStyle = .capsule
        button.baseBackgroundColor = .systemBlue
        button.baseForegroundColor = .white
        button.contentInsets = NSDirectionalEdgeInsets(top: 12, leading: 24, bottom: 12, trailing: 24)
        reset.configuration = button
        reset.addTarget(self, action: #selector(startOver), for: .touchUpInside)
        reset.accessibilityIdentifier = "reset-corrupt-store"
        error.font = .preferredFont(forTextStyle: .footnote)
        error.textColor = .systemRed
        error.numberOfLines = 0
        error.adjustsFontForContentSizeCategory = true
        error.isHidden = true
        let content = UIStackView(arrangedSubviews: [heading, detail, reset, error])
        content.axis = .vertical
        content.alignment = .leading
        content.spacing = 24
        content.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(content)
        NSLayoutConstraint.activate([
            content.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 32),
            content.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -32),
            content.centerYAnchor.constraint(equalTo: view.safeAreaLayoutGuide.centerYAnchor),
            reset.heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
    }

    @objc private func startOver() {
        reset.isEnabled = false
        error.isHidden = true
        EngineHost.shared.resetCorruptStore { [weak self] result in
            guard let self else { return }
            switch result {
            case .success:
                self.navigationController?.setViewControllers([SessionsController()], animated: false)
            case .failure(let failure):
                self.error.text = "Couldn’t clear the local store: \(failure.localizedDescription)"
                self.error.isHidden = false
                self.reset.isEnabled = true
            }
        }
    }
}
