import UIKit

protocol DatabaseDraft {
    var hasDatabaseDraft: Bool { get }
}

enum DatabaseBrowser {
    private static let nameLocale = Locale(identifier: "und")

    // Foundation's root-locale ICU collation implements UCA for display names.
    static func namedSchemaOrder(_ a: (key: String, value: [String: Any]),
                                 _ b: (key: String, value: [String: Any])) -> Bool {
        let order = (a.value["name"] as? String ?? a.key).compare(
            b.value["name"] as? String ?? b.key, locale: nameLocale)
        return order == .orderedSame ? a.key < b.key : order == .orderedAscending
    }

    static func call(_ value: [String: Any], completion: @escaping (Result<[String: Any], Error>) -> Void) {
        do {
            let data = try JSONSerialization.data(withJSONObject: value)
            EngineHost.shared.request(.database(data), completion: completion)
        } catch { completion(.failure(error)) }
    }

    static func reference(volume: String, entry: String) -> String {
        "playbit:database:\(volume):\(entry)\n"
    }

    static func address(_ volume: String, _ entry: String) -> [String: String] {
        ["volume": volume, "entry": entry]
    }

    static func hex(_ bytes: [UInt8]) -> String {
        bytes.map { String(format: "%02x", $0) }.joined()
    }

    static func unhex(_ text: String) -> [UInt8]? {
        let text = text.filter { !$0.isWhitespace }
        guard text.utf8.count <= 32 * 1024, text.count % 2 == 0 else { return nil }
        var result: [UInt8] = []
        var index = text.startIndex
        while index < text.endIndex {
            let end = text.index(index, offsetBy: 2)
            guard let byte = UInt8(text[index..<end], radix: 16) else { return nil }
            result.append(byte)
            index = end
        }
        return result
    }
}

class DatabaseTableController: UITableViewController, DatabaseDraft {
    let message = UILabel()
    var mutationPending = false
    var hasDatabaseDraft: Bool { mutationPending }
    private var refreshNeeded = false
    var busy = false {
        didSet {
            if !busy {
                refreshControl?.endRefreshing()
                if refreshNeeded {
                    refreshNeeded = false
                    DispatchQueue.main.async { [weak self] in
                        guard let self, self.navigationController?.topViewController === self else { return }
                        self.reload()
                    }
                }
            }
        }
    }
    private var changed: NSObjectProtocol?

    init() { super.init(style: .insetGrouped) }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }

    override func viewDidLoad() {
        super.viewDidLoad()
        message.numberOfLines = 0
        message.textAlignment = .center
        message.font = .preferredFont(forTextStyle: .body)
        message.textColor = .secondaryLabel
        message.adjustsFontForContentSizeCategory = true
        tableView.backgroundView = message
        tableView.accessibilityIdentifier = "volume-browser"
        refreshControl = UIRefreshControl()
        refreshControl?.addTarget(self, action: #selector(requestRefresh), for: .valueChanged)
        changed = NotificationCenter.default.addObserver(forName: EngineHost.backgroundChanged,
            object: nil, queue: .main) { [weak self] _ in
                guard let self, self.navigationController?.topViewController === self else { return }
                self.requestRefresh()
            }
    }
    deinit { if let changed { NotificationCenter.default.removeObserver(changed) } }
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        reload()
    }
    func reload() {}
    @objc private func requestRefresh() {
        if busy { refreshNeeded = true }
        else { reload() }
    }
    func show(_ error: Error) {
        message.text = error.localizedDescription
        tableView.reloadData()
    }
    func alert(_ text: String) {
        let alert = UIAlertController(title: "Database", message: text, preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "OK", style: .default))
        present(alert, animated: true)
    }
    func cell(_ title: String, detail: String? = nil, disclosure: Bool = true) -> UITableViewCell {
        let cell = UITableViewCell(style: .subtitle, reuseIdentifier: nil)
        cell.textLabel?.text = title
        cell.detailTextLabel?.text = detail
        cell.textLabel?.font = .preferredFont(forTextStyle: .body)
        cell.textLabel?.adjustsFontForContentSizeCategory = true
        cell.detailTextLabel?.adjustsFontForContentSizeCategory = true
        cell.accessoryType = disclosure ? .disclosureIndicator : .none
        return cell
    }
}

final class VolumeBrowserController: DatabaseTableController {
    private let volume: String?
    private let parentID: String?
    private var rows: [[String: Any]] = []
    private var nextPage: String?
    private var after: String?
    var selectedDatabase: String?

    init(volume: String? = nil, parent: String? = nil, name: String = "Volumes") {
        self.volume = volume
        self.parentID = parent
        super.init()
        title = name
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }

    func seek(to entry: String) {
        guard var bytes = Identifier.decode(entry, bytes: 15) else { return }
        for index in bytes.indices.reversed() {
            let old = bytes[index]
            bytes[index] = old &- 1
            if old != 0 { break }
        }
        after = bytes.allSatisfy { $0 == 0 } ? nil : Identifier.encode(bytes)
    }

    override func reload() {
        guard !busy else { return }
        busy = true
        var request: [String: Any] = ["op": "volumes"]
        if let volume {
            request = ["op": "entries", "volume": volume]
            if let parent = parentID { request["parent"] = parent }
            if let after { request["after"] = after }
        }
        if rows.isEmpty { message.text = "Loading…" }
        DatabaseBrowser.call(request) { [weak self] result in
            guard let self else { return }
            self.busy = false
            switch result {
            case .success(let value):
                if let page = value["page"] as? [String: Any] {
                    self.rows = page["items"] as? [[String: Any]] ?? []
                    self.nextPage = page["next"] as? String
                } else { self.rows = value["volumes"] as? [[String: Any]] ?? [] }
                self.message.text = self.rows.isEmpty ? "No entries" : nil
                self.tableView.reloadData()
            case .failure(let error): self.rows = []; self.show(error)
            }
        }
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        rows.count + (nextPage == nil ? 0 : 1) + (after == nil ? 0 : 1)
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        guard path.row < rows.count else {
            return cell(path.row == rows.count && nextPage != nil ? "Next page" : "First page", disclosure: false)
        }
        let row = rows[path.row]
        let cell = cell(row["filename"] as? String ?? row["name"] as? String ?? "Entry")
        let kind = row["kind"] as? String
        cell.imageView?.image = UIImage(systemName: volume == nil ? "externaldrive" : kind == "directory" ? "folder" : kind == "blob" ? "doc" : "tablecells")
        if kind == "blob" { cell.accessoryType = .none }
        if row["id"] as? String == selectedDatabase {
            cell.accessoryType = .checkmark
            cell.accessibilityTraits.insert(.selected)
        }
        return cell
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !busy else { return }
        guard path.row < rows.count else {
            after = path.row == rows.count && nextPage != nil ? nextPage : nil
            reload()
            return
        }
        let row = rows[path.row]
        guard let id = row["id"] as? String else { return }
        let name = row["filename"] as? String ?? row["name"] as? String ?? "Volume"
        guard let volume else {
            navigationController?.pushViewController(VolumeBrowserController(volume: id, name: name), animated: true)
            return
        }
        if row["kind"] as? String == "directory" {
            navigationController?.pushViewController(VolumeBrowserController(volume: volume, parent: id, name: name), animated: true)
        } else if row["kind"] as? String != "blob" {
            selectedDatabase = id
            tableView.reloadData()
            navigationController?.pushViewController(DatabaseController(reference: DatabaseBrowser.reference(volume: volume, entry: id)), animated: true)
        } else {
            alert("File opening is not available in this iOS browser yet.")
        }
    }
}

final class DatabaseController: DatabaseTableController {
    var opened: ((String, String, String, String?) -> Void)?
    private let reference: String
    private var address: [String: String] = [:]
    private var types: [(String, [String: Any])] = []
    private var keys: [[String: Any]] = []
    private var isKeyValue = false
    private var writable = false
    private var nextPage: [UInt8]?
    private var after: [UInt8]?
    private var root: [String: Any]?

    init(reference: String) {
        self.reference = reference
        super.init()
        title = "Database"
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }

    override func reload() {
        guard !busy else { return }
        busy = true
        DatabaseBrowser.call(["op": "open", "reference": reference]) { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let value):
                guard let volume = value["volume"] as? [String: Any], let volumeID = volume["id"] as? String,
                      let details = value["details"] as? [String: Any],
                      let entry = details["entry"] as? [String: Any], let id = entry["id"] as? String,
                      let info = entry["info"] as? [String: Any], let kind = info["kind"] as? [String: Any] else {
                    self.busy = false
                    self.alert("The database response could not be read.")
                    return
                }
                self.address = DatabaseBrowser.address(volumeID, id)
                if let opened = self.opened {
                    self.opened = nil
                    let parent = value["parent"] as? String
                    opened(volumeID, volume["name"] as? String ?? "Volume", id, parent)
                }
                self.title = info["name"] as? String ?? "Database"
                self.writable = details["writable"] as? Bool ?? false
                self.navigationItem.prompt = self.writable ? nil : "Read only"
                if info["protocol"] as? String == "blob_log" {
                    self.isKeyValue = false; self.keys = []; self.types = []
                    self.message.text = "Append log editing is not available in this browser."
                    self.navigationItem.rightBarButtonItem = nil
                    self.busy = false
                    self.tableView.reloadData()
                    return
                }
                self.isKeyValue = kind["type"] as? String == "kv_db"
                if self.isKeyValue { self.loadKeys() }
                else {
                    let schema = kind["schema"] as? [String: Any] ?? [:]
                    let types = schema["types"] as? [String: [String: Any]] ?? [:]
                    self.types = types.sorted(by: DatabaseBrowser.namedSchemaOrder)
                    self.message.text = self.types.isEmpty ? "No record types" : nil
                    self.busy = false
                    self.tableView.reloadData()
                }
                self.navigationItem.rightBarButtonItem = self.isKeyValue && self.writable
                    ? UIBarButtonItem(barButtonSystemItem: .add, target: self, action: #selector(self.addKey)) : nil
            case .failure(let error): self.busy = false; self.keys = []; self.types = []; self.show(error)
            }
        }
    }
    private func loadKeys() {
        busy = true
        var request: [String: Any] = ["op": "keys", "address": address]
        if let after { request["after"] = after; request["root"] = root }
        DatabaseBrowser.call(request) { [weak self] result in
            guard let self else { return }
            self.busy = false
            switch result {
            case .success(let value):
                let page = value["page"] as? [String: Any] ?? [:]
                self.keys = page["keys"] as? [[String: Any]] ?? []
                self.nextPage = page["next"] as? [UInt8]
                self.root = page["root"] as? [String: Any]
                self.message.text = self.keys.isEmpty ? "No keys" : nil
                self.tableView.reloadData()
            case .failure(let error): self.keys = []; self.nextPage = nil; self.after = nil; self.root = nil; self.show(error)
            }
        }
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        isKeyValue ? keys.count + (nextPage == nil ? 0 : 1) + (after == nil ? 0 : 1) : types.count
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        if !isKeyValue { return cell(types[path.row].1["name"] as? String ?? types[path.row].0) }
        guard path.row < keys.count else { return cell(path.row == keys.count && nextPage != nil ? "Next page" : "First page", disclosure: false) }
        let key = keys[path.row]["bytes"] as? [UInt8] ?? []
        return cell(String(bytes: key, encoding: .utf8) ?? DatabaseBrowser.hex(key), detail: "\(keys[path.row]["len_bytes"] ?? 0) bytes")
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !busy else { return }
        if !isKeyValue {
            let type = types[path.row]
            navigationController?.pushViewController(DatabaseRecordsController(address: address, type: type.0, declaration: type.1, writable: writable), animated: true)
        } else if path.row >= keys.count {
            after = path.row == keys.count && nextPage != nil ? nextPage : nil
            loadKeys()
        } else if let key = keys[path.row]["bytes"] as? [UInt8] {
            navigationController?.pushViewController(DatabaseValueController(address: address, key: key, writable: writable), animated: true)
        }
    }
    @objc private func addKey() {
        let alert = UIAlertController(title: "New key", message: nil, preferredStyle: .alert)
        alert.addTextField { $0.placeholder = "Key"; $0.autocapitalizationType = .none }
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Create", style: .default) { [weak self, weak alert] _ in
            guard let self, let name = alert?.textFields?.first?.text, !name.isEmpty else { return }
            guard name.utf8.count <= 511 else { self.alert("A key must contain at most 511 UTF-8 bytes."); return }
            let key = Array(name.utf8)
            self.navigationController?.pushViewController(DatabaseValueController(address: self.address, key: key, writable: true, creating: true), animated: true)
        })
        present(alert, animated: true)
    }
}

final class DatabaseValueController: UIViewController, DatabaseDraft, UITextViewDelegate {
    private let address: [String: String]
    private let key: [UInt8]
    private let writable: Bool
    private let creating: Bool
    private let editor = UITextView()
    private let encoding = UISegmentedControl(items: ["Text", "Hex"])
    private let message = UILabel()
    private var bytes: [UInt8] = []
    private var mode = 0
    private var busy = false
    var hasDatabaseDraft: Bool { busy || editedBytes() != bytes }

    init(address: [String: String], key: [UInt8], writable: Bool, creating: Bool = false) {
        self.address = address; self.key = key; self.writable = writable; self.creating = creating
        super.init(nibName: nil, bundle: nil)
        title = String(bytes: key, encoding: .utf8) ?? DatabaseBrowser.hex(key)
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        editor.font = .preferredFont(forTextStyle: .body)
        editor.adjustsFontForContentSizeCategory = true
        editor.autocorrectionType = .no
        editor.autocapitalizationType = .none
        editor.accessibilityIdentifier = "database-value"
        editor.delegate = self
        message.numberOfLines = 0
        message.font = .preferredFont(forTextStyle: .footnote)
        message.textColor = .secondaryLabel
        encoding.selectedSegmentIndex = 0
        encoding.addTarget(self, action: #selector(changeEncoding), for: .valueChanged)
        let content = UIStackView(arrangedSubviews: [encoding, editor, message])
        content.axis = .vertical; content.spacing = 12
        content.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(content)
        NSLayoutConstraint.activate([
            content.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
            content.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 16),
            content.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            content.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor, constant: -16)
        ])
        navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .save, target: self, action: #selector(save))
        if writable && !creating, let save = navigationItem.rightBarButtonItem {
            navigationItem.rightBarButtonItems = [save, UIBarButtonItem(barButtonSystemItem: .trash, target: self, action: #selector(deleteKey))]
        }
        navigationItem.rightBarButtonItem?.isEnabled = false
        isModalInPresentation = true
        if creating { configure([]); return }
        DatabaseBrowser.call(["op": "read", "address": address, "key": key]) { [weak self] result in
            guard let self else { return }
            switch result {
            case .success(let reply):
                let value = reply["value"] as? [String: Any] ?? [:]
                let bytes = value["bytes"] as? [UInt8] ?? []
                self.configure(bytes)
                if (value["total_bytes"] as? NSNumber)?.uint64Value != UInt64(bytes.count) {
                    self.editor.isEditable = false
                    self.navigationItem.rightBarButtonItem?.isEnabled = false
                    self.message.text = "Showing the first 16 KiB. Editing this large value is not available here."
                }
            case .failure(let error): self.editor.isEditable = false; self.message.text = error.localizedDescription
            }
        }
    }
    private func configure(_ bytes: [UInt8]) {
        self.bytes = bytes
        let text = String(bytes: bytes, encoding: .utf8).flatMap { text -> String? in
            text.unicodeScalars.allSatisfy {
                !CharacterSet.controlCharacters.contains($0) || [9, 10, 13].contains($0.value)
            } ? text : nil
        }
        mode = text == nil ? 1 : 0
        encoding.selectedSegmentIndex = mode
        editor.text = mode == 0 ? text : DatabaseBrowser.hex(bytes)
        editor.isEditable = writable
        navigationItem.rightBarButtonItem?.isEnabled = writable
    }
    private func editedBytes() -> [UInt8]? {
        mode == 0 ? Array(editor.text.utf8) : DatabaseBrowser.unhex(editor.text)
    }
    func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText text: String) -> Bool {
        guard text.utf8.count <= 32 * 1024 else { return false }
        let value = (textView.text as NSString).replacingCharacters(in: range, with: text)
        return value.utf8.count <= (mode == 0 ? 16 * 1024 : 32 * 1024)
    }
    @objc private func deleteKey() {
        guard !busy else { return }
        let alert = UIAlertController(title: "Delete key?", message: "This removes this key and its value.", preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak self] _ in
            guard let self else { return }
            self.busy = true
            DatabaseBrowser.call(["op": "delete_key", "address": self.address, "key": self.key]) { result in
                self.busy = false
                switch result {
                case .success: self.navigationController?.popViewController(animated: true)
                case .failure(let error): self.message.text = error.localizedDescription
                }
            }
        })
        present(alert, animated: true)
    }
    @objc private func changeEncoding() {
        guard let bytes = editedBytes() else { encoding.selectedSegmentIndex = mode; message.text = "Enter an even number of hexadecimal digits."; return }
        let target = encoding.selectedSegmentIndex
        if target == 0, String(bytes: bytes, encoding: .utf8) == nil {
            encoding.selectedSegmentIndex = mode; message.text = "These bytes are not UTF-8 text."; return
        }
        mode = target
        editor.text = mode == 0 ? String(bytes: bytes, encoding: .utf8) : DatabaseBrowser.hex(bytes)
        message.text = nil
    }
    @objc private func save() {
        guard !busy, let bytes = editedBytes() else { return }
        busy = true; navigationItem.rightBarButtonItem?.isEnabled = false
        DatabaseBrowser.call(["op": creating ? "insert" : "put", "address": address, "key": key, "value": bytes]) { [weak self] result in
            guard let self else { return }
            self.busy = false; self.navigationItem.rightBarButtonItem?.isEnabled = true
            switch result {
            case .success: self.navigationController?.popViewController(animated: true)
            case .failure(let error): self.message.text = error.localizedDescription
            }
        }
    }
}
