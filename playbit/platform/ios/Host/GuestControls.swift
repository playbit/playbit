import UIKit

// Keep UIKit's editor identity and native selection/IME implementation across
// guest scene updates. A reply may only replace the revision it acknowledges.
final class GuestTextField: UITextField {
    var readOnly = false {
        didSet { if readOnly && !oldValue { revision.acknowledge(revision.value) } }
    }
    var revision = GuestEditRevision()
    var edits = GuestTextEdits()
    override init(frame: CGRect) {
        super.init(frame: frame)
        borderStyle = .roundedRect
        font = .preferredFont(forTextStyle: .body)
        adjustsFontForContentSizeCategory = true
        heightAnchor.constraint(greaterThanOrEqualToConstant: 44).isActive = true
        setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
    }
    required init?(coder: NSCoder) { fatalError() }
    func apply(value: String, acknowledgment: UInt64?) {
        guard markedTextRange == nil, revision.accepts(acknowledgment), text != value else { return }
        // Assigning equal text would needlessly disturb native selection.
        text = value
        edits.reset()
    }
}

// UIKit owns the control's touch tracking, highlighting, focus, typography and
// disabled appearance. iOS has no tri-state checkbox class; a native button
// presents the system checkbox symbols and exposes its current state to AX.
final class GuestControl: UIButton {
    override init(frame: CGRect) {
        super.init(frame: frame)
        heightAnchor.constraint(greaterThanOrEqualToConstant: 44).isActive = true
    }
    required init?(coder: NSCoder) { fatalError() }
    func apply(_ node: GuestScene) {
        let checkbox = node.kind == .checkbox
        let state = node.state
        let style = node.style
        var config: UIButton.Configuration = checkbox ? .plain() : style == 1 ? .filled() : .tinted()
        config.title = node.text
        if checkbox {
            config.image = UIImage(systemName: state == 2 ? "minus.square" : state == 1 ? "checkmark.square" : "square")
            config.imagePadding = 8
            contentHorizontalAlignment = .leading
        } else { contentHorizontalAlignment = .center }
        if style == 2 && !checkbox { config.baseBackgroundColor = .systemRed; config.baseForegroundColor = .systemRed }
        configuration = config
        isEnabled = !node.disabled
        accessibilityLabel = config.title
        accessibilityTraits = [.button]
        if !isEnabled { accessibilityTraits.insert(.notEnabled) }
        if checkbox {
            accessibilityValue = state == 2 ? "Mixed" : state == 1 ? "Done" : "Not done"
            if state == 1 { accessibilityTraits.insert(.selected) }
        } else { accessibilityValue = nil }
    }
}
