import UIKit

final class DatabaseRecordsController: DatabaseTableController {
    private let address: [String: String]
    private let type: String
    private let declaration: [String: Any]
    private let writable: Bool
    private var rows: [[String: Any]] = []
    private var after: String?
    private var nextPage: String?

    init(address: [String: String], type: String, declaration: [String: Any], writable: Bool) {
        self.address = address; self.type = type; self.declaration = declaration; self.writable = writable
        super.init()
        title = declaration["name"] as? String ?? "Records"
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func viewDidLoad() {
        super.viewDidLoad()
        if writable { navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .add, target: self, action: #selector(addRecord)) }
    }
    override func reload() {
        guard !busy else { return }
        busy = true
        var request: [String: Any] = ["op": "records", "address": address, "type_id": type]
        if let after { request["after"] = after }
        DatabaseBrowser.call(request) { [weak self] result in
            guard let self else { return }
            self.busy = false
            switch result {
            case .success(let value):
                let page = value["page"] as? [String: Any] ?? [:]
                self.rows = page["objects"] as? [[String: Any]] ?? []
                self.nextPage = page["next"] as? String
                self.message.text = self.rows.isEmpty ? "No records" : nil
                self.tableView.reloadData()
            case .failure(let error): self.rows = []; self.show(error)
            }
        }
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        rows.count + (nextPage == nil ? 0 : 1) + (after == nil ? 0 : 1)
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        guard path.row < rows.count else { return cell(path.row == rows.count && nextPage != nil ? "Next page" : "First page", disclosure: false) }
        let record = rows[path.row]
        let fields = record["fields"] as? [String: [String: Any]] ?? [:]
        let text = fields.sorted { $0.key < $1.key }.compactMap { field -> String? in
            guard let value = (field.value["values"] as? [[String: Any]])?.first,
                  value["kind"] as? String == "text" else { return nil }
            return value["value"] as? String
        }.first
        return cell(text.flatMap { $0.isEmpty ? nil : $0 } ?? record["object_id"] as? String ?? "Record")
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !busy else { return }
        guard path.row < rows.count else {
            after = path.row == rows.count && nextPage != nil ? nextPage : nil
            reload(); return
        }
        open(rows[path.row])
    }
    private func open(_ record: [String: Any]) {
        navigationController?.pushViewController(DatabaseRecordController(address: address,
            record: record, declaration: declaration, writable: writable), animated: true)
    }
    @objc private func addRecord() {
        guard !busy else { return }
        busy = true
        mutationPending = true
        DatabaseBrowser.call(["op": "create", "address": address, "type_id": type]) { [weak self] result in
            guard let self else { return }
            self.busy = false
            self.mutationPending = false
            switch result {
            case .success(let reply): if let record = reply["record"] as? [String: Any] { self.open(record) }
            case .failure(let error): self.alert(error.localizedDescription)
            }
        }
    }
}

final class DatabaseRecordController: DatabaseTableController {
    private let address: [String: String]
    private var record: [String: Any]
    private let writable: Bool
    private let fields: [(String, [String: Any])]

    init(address: [String: String], record: [String: Any], declaration: [String: Any], writable: Bool) {
        self.address = address; self.record = record; self.writable = writable
        fields = (declaration["fields"] as? [String: [String: Any]] ?? [:])
            .sorted(by: DatabaseBrowser.namedSchemaOrder)
        super.init()
        title = "Record"
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func viewDidLoad() {
        super.viewDidLoad()
        if writable { navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .trash, target: self, action: #selector(deleteRecord)) }
    }
    override func reload() {
        guard !busy, let object = record["object_id"] as? String else { return }
        busy = true
        DatabaseBrowser.call(["op": "record", "address": address, "object": object]) { [weak self] result in
            guard let self else { return }
            self.busy = false
            switch result {
            case .success(let reply): self.record = reply["record"] as? [String: Any] ?? [:]; self.message.text = nil; self.tableView.reloadData()
            case .failure(let error): self.record = [:]; self.show(error)
            }
        }
    }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
        record.isEmpty ? 0 : fields.count
    }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let field = fields[path.row]
        let stored = record["fields"] as? [String: [String: Any]] ?? [:]
        let values = stored[field.0]?["values"] as? [[String: Any]] ?? []
        return cell(field.1["name"] as? String ?? field.0,
            detail: values.isEmpty ? "Empty" : values.map(DatabaseFieldController.caption).joined(separator: ", "), disclosure: writable)
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard writable, let object = record["object_id"] as? String else { return }
        let field = fields[path.row]
        let stored = record["fields"] as? [String: [String: Any]] ?? [:]
        let editor = DatabaseFieldController(declaration: field.1, values: stored[field.0]?["values"] as? [[String: Any]] ?? [])
        editor.commit = { [weak self] values, done in
            guard let self else { return }
            let fields: [String: Any] = [field.0: values.isEmpty ? NSNull() : values]
            DatabaseBrowser.call(["op": "edit", "address": self.address, "object": object, "fields": fields]) { result in
                switch result {
                case .success(let reply): self.record = reply["record"] as? [String: Any] ?? self.record; done(nil)
                case .failure(let error): done(error)
                }
            }
        }
        navigationController?.pushViewController(editor, animated: true)
    }
    @objc private func deleteRecord() {
        guard !busy, let object = record["object_id"] as? String else { return }
        let alert = UIAlertController(title: "Delete record?", message: "This removes the record from this database.", preferredStyle: .alert)
        alert.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        alert.addAction(UIAlertAction(title: "Delete", style: .destructive) { [weak self] _ in
            guard let self else { return }
            self.busy = true
            self.mutationPending = true
            DatabaseBrowser.call(["op": "delete_record", "address": self.address, "object": object]) { result in
                self.busy = false
                self.mutationPending = false
                switch result {
                case .success: self.navigationController?.popViewController(animated: true)
                case .failure(let error): self.alert(error.localizedDescription)
                }
            }
        })
        present(alert, animated: true)
    }
}

final class DatabaseFieldController: UITableViewController, DatabaseDraft {
    private let declaration: [String: Any]
    private var values: [[String: Any]]
    private var busy = false
    private let initialValues: NSArray
    var hasDatabaseDraft: Bool { busy || !initialValues.isEqual(to: values) }
    var commit: (([[String: Any]], @escaping (Error?) -> Void) -> Void)?

    init(declaration: [String: Any], values: [[String: Any]]) {
        self.declaration = declaration; self.values = values
        self.initialValues = values as NSArray
        super.init(style: .insetGrouped)
        title = declaration["name"] as? String ?? "Field"
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func viewDidLoad() {
        super.viewDidLoad()
        navigationItem.hidesBackButton = true
        navigationItem.leftBarButtonItem = UIBarButtonItem(barButtonSystemItem: .cancel, target: self, action: #selector(cancel))
        navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .save, target: self, action: #selector(save))
    }
    static func caption(_ value: [String: Any]) -> String {
        switch value["kind"] as? String {
        case "text": return value["value"] as? String ?? ""
        case "boolean": return value["value"] as? Bool == true ? "true" : "false"
        case "bytes": return "\((value["value"] as? [UInt8] ?? []).count) bytes"
        case "file": return "File reference"
        case "reference": return "Record reference"
        default: return "\(value["value"] ?? "")"
        }
    }
    private var canAdd: Bool { values.count < 256 && (values.isEmpty || declaration["repeated"] as? Bool == true) }
    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { values.count + (canAdd ? 1 : 0) }
    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let cell = UITableViewCell(style: .default, reuseIdentifier: nil)
        cell.textLabel?.text = path.row == values.count ? "Add value" : Self.caption(values[path.row])
        cell.textLabel?.numberOfLines = 3
        cell.accessoryType = .disclosureIndicator
        return cell
    }
    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        guard !busy else { return }
        let kind = declaration["value_type"] as? [String: Any] ?? [:]
        let editor = DatabaseScalarController(kind: kind["kind"] as? String ?? "text", value: path.row < values.count ? values[path.row] : nil)
        editor.commit = { [weak self] value in
            guard let self else { return "This field editor is closed." }
            var values = self.values
            if path.row == values.count { values.append(value) }
            else { values[path.row] = value }
            guard let encoded = try? JSONSerialization.data(withJSONObject: values), encoded.count <= 32 * 1024 else {
                return "These field values exceed this editor’s size limit."
            }
            self.values = values
            self.tableView.reloadData()
            return nil
        }
        navigationController?.pushViewController(editor, animated: true)
    }
    override func tableView(_ tableView: UITableView, canEditRowAt path: IndexPath) -> Bool { !busy && path.row < values.count }
    override func tableView(_ tableView: UITableView, commit style: UITableViewCell.EditingStyle, forRowAt path: IndexPath) {
        guard style == .delete else { return }
        values.remove(at: path.row); tableView.reloadData()
    }
    @objc private func cancel() { guard !busy else { return }; navigationController?.popViewController(animated: true) }
    @objc private func save() {
        guard !busy else { return }
        guard let encoded = try? JSONSerialization.data(withJSONObject: values), encoded.count <= 32 * 1024 else {
            let alert = UIAlertController(title: "Couldn’t save", message: "These field values exceed this editor’s size limit. Reduce the values or cancel this edit.", preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "OK", style: .default)); present(alert, animated: true)
            return
        }
        busy = true; navigationItem.rightBarButtonItem?.isEnabled = false
        commit?(values) { [weak self] error in
            guard let self else { return }
            self.busy = false; self.navigationItem.rightBarButtonItem?.isEnabled = true
            if let error {
                let alert = UIAlertController(title: "Couldn’t save", message: error.localizedDescription, preferredStyle: .alert)
                alert.addAction(UIAlertAction(title: "OK", style: .default)); self.present(alert, animated: true)
            } else { self.navigationController?.popViewController(animated: true) }
        }
    }
}

final class DatabaseScalarController: UIViewController, DatabaseDraft, UITextViewDelegate {
    private let kind: String
    private let initial: [String: Any]?
    private let text = UITextView()
    private let hint = UILabel()
    private var initialText = ""
    var hasDatabaseDraft: Bool { isViewLoaded && text.text != initialText }
    var commit: (([String: Any]) -> String?)?

    init(kind: String, value: [String: Any]?) {
        self.kind = kind; self.initial = value
        super.init(nibName: nil, bundle: nil)
        title = kind.capitalized
    }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        text.font = .preferredFont(forTextStyle: .body)
        text.accessibilityIdentifier = "database-field-value"
        text.adjustsFontForContentSizeCategory = true
        text.autocorrectionType = .no
        text.autocapitalizationType = .none
        text.delegate = self
        hint.numberOfLines = 0; hint.font = .preferredFont(forTextStyle: .footnote); hint.textColor = .secondaryLabel
        switch kind {
        case "bytes": text.text = DatabaseBrowser.hex(initial?["value"] as? [UInt8] ?? []); hint.text = "Hexadecimal bytes"
        case "file", "reference":
            let value = initial?["value"] as? [String: String] ?? [:]
            text.text = [value["volume_id"], value[kind == "file" ? "entry_id" : "database_id"], value["object_id"]].compactMap { $0 }.joined(separator: ":")
            hint.text = kind == "file" ? "Volume ID : file entry ID" : "Volume ID : database entry ID : record ID"
        case "boolean": text.text = (initial?["value"] as? Bool ?? false) ? "true" : "false"; hint.text = "true or false"
        case "text": text.text = initial?["value"] as? String ?? ""
        default: text.text = "\(initial?["value"] ?? 0)"
        }
        initialText = text.text
        let content = UIStackView(arrangedSubviews: [hint, text]); content.axis = .vertical; content.spacing = 12; content.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(content)
        NSLayoutConstraint.activate([
            content.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
            content.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 16),
            content.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            content.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor, constant: -16)
        ])
        navigationItem.rightBarButtonItem = UIBarButtonItem(barButtonSystemItem: .done, target: self, action: #selector(done))
    }
    func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText text: String) -> Bool {
        guard text.utf8.count <= 16 * 1024 else { return false }
        return (textView.text as NSString).replacingCharacters(in: range, with: text).utf8.count <= 16 * 1024
    }
    @objc private func done() {
        let input = text.text ?? ""
        var value: Any
        switch kind {
        case "text": value = input
        case "boolean": guard input == "true" || input == "false" else { hint.text = "Enter true or false."; return }; value = input == "true"
        case "integer": guard let number = Int64(input) else { hint.text = "Enter a 64-bit integer."; return }; value = number
        case "float": guard let number = Double(input), number.isFinite else { hint.text = "Enter a finite number."; return }; value = number
        case "bytes": guard let bytes = DatabaseBrowser.unhex(input) else { hint.text = "Enter an even number of hexadecimal digits."; return }; value = bytes
        case "file", "reference":
            let parts = input.split(separator: ":", omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            guard parts.count == (kind == "file" ? 2 : 3) else { hint.text = "Enter the IDs separated by colons."; return }
            var reference = ["volume_id": parts[0], kind == "file" ? "entry_id" : "database_id": parts[1]]
            if kind == "reference" { reference["object_id"] = parts[2] }
            value = reference
        default: return
        }
        guard input.utf8.count <= 16 * 1024 else { hint.text = "This editor supports values up to 16 KiB."; return }
        if let error = commit?(["kind": kind, "value": value]) { hint.text = error; return }
        navigationController?.popViewController(animated: true)
    }
}
