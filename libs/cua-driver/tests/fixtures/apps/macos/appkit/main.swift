// CuaTestHarness.AppKit — deterministic Cocoa AppKit host app for the
// cua-driver-rs test harness. Mirrors the role of CuaTestHarness.Wpf.
//
// Single-file Swift, compiled with swiftc against AppKit. Wrapped in a
// minimal .app bundle by build.sh so AX / TCC behaves like a real app.
//
// Scenarios covered (see ../scenarios/scenarios.json `appkit` section):
//   counter        — NSButton increments NSTextField counter
//   text_body      — NSTextField with the shared HARNESS_TEXT_MARKER_v1
//   text_input     — NSTextField with a mirror label for type_text / set_value
//                    and an opt-in controlled child-process command oracle
//   click_target   — NSButton (AX-addressable) records click/double_click/right_click
//   slider         — NSSlider drives drag / set_value (slider_value=)
//   checkable_controls — NSButton checkbox (agreed=)
//   context_menu   — NSButton + NSMenu (Cut/Copy/Paste → menu_action=)
//   scroll_target  — NSScrollView with a tall body and offset label
//   ns_menubar     — main menu item with known title (Mac-specific)
//   child_editor   — a borderless child window accessibility publishes as its
//                    own text field, drawn inside the main window, plus an
//                    attached sheet (CUA_APPKIT_CHILD_EDITOR=1 | sheet)
//   exit           — NSButton terminates the app
//
// AX identifiers (via `setAccessibilityIdentifier(_:)`) match the IDs in
// scenarios.json. Window title is set to "CuaTestHarness AppKit" so the
// Rust tests can find it via NSWorkspace / accessibility queries.

import AppKit

// MARK: - Constants (must match ../scenarios/scenarios.json `appkit`)
let kWindowTitle = "CuaTestHarness AppKit"
let kWindowAID = "wnd-main"
let kIncrementButtonAID = "btn-increment"
let kCounterLabelAID = "lbl-counter"
let kResetButtonAID = "btn-reset"
let kTextBodyAID = "txt-body"
let kTextBodyMarker = "HARNESS_TEXT_MARKER_v1"
let kTextInputAID = "txt-input"
let kTextInputMirrorAID = "lbl-input-mirror"
let kTextInputCommitAID = "lbl-input-commit"
let kClickTargetAID = "btn-clicktarget"
let kLastActionAID = "lbl-last-action"
let kClickCountAID = "lbl-click-count"
let kSliderAID = "sld-value"
let kSliderValueAID = "lbl-slider-value"
let kCheckboxAID = "chk-agree"
let kCheckStateAID = "lbl-chk-state"
let kSelectionStateAID = "lbl-selection-state"
let kPressableRowAID = "row-pressable"
let kPressableRowStateAID = "lbl-pressable-row-state"
let kRowHelpGroupAID = "grp-row-help"
let kContextButtonAID = "btn-context"
let kMenuActionAID = "lbl-menu-action"
let kScrollerAID = "scroll-tall"
let kScrollOffsetAID = "lbl-scroll-offset"
let kAccelCountAID = "lbl-accel-count"
let kScrollTopMarker = "SCROLL_TOP_MARKER_v1"
let kScrollBottomMarker = "SCROLL_BOTTOM_MARKER_v1"
let kExitButtonAID = "btn-exit"
let kMenuItemTitle = "Harness Test Item"
let kSecondaryWindowTitle = "CuaTestHarness AppKit Secondary"
let kSheetWindowTitle = "CuaTestHarness AppKit Sheet"
let kFloatingWindowTitle = "CuaTestHarness AppKit Floating"
/// Not a `kWindowTitle` substring: the harness finds its main window by that.
let kSecondKeyWindowTitle = "CuaTestHarness Second Key Window"
/// end_of_edit — five editable fields that differ only in what their app does
/// with its own end-of-edit. Each placeholder doubles as the field's stable AX
/// label: the one description an edit cannot change, so a control can be found
/// again after its app rebuilds it.
let kReformatInputAID = "txt-reformat"
let kReformatPlaceholder = "Reformats on commit"
let kSwapInputAID = "txt-swap"
let kSwapPlaceholder = "Swapped on commit"
let kDiscardInputAID = "txt-discard"
let kDiscardPlaceholder = "Discards on commit"
let kDiscardKeptValue = "keep-me"
let kConfirmInputAID = "txt-confirm"
let kConfirmPlaceholder = "Confirms its own commit"
let kNonBmpInputAID = "txt-nonbmp"
let kNonBmpPlaceholder = "Non-BMP seed"
/// Four UTF-16 units in three characters: a selection measured in characters
/// covers three of them and leaves the "B" for typed text to be appended to.
let kNonBmpSeedValue = "\u{1F600}AB"
let kEndOfEditStateAID = "lbl-end-of-edit"
let kRowNameCellAID = "txt-row-name"
let kRowNameCellValue = "row name cell"
let kFirstResponderAID = "lbl-first-responder"
/// menu_popover — a popup button whose press opens a native NSMenu, and a
/// button that shows an NSPopover tall enough to hang past the window's
/// bottom edge (`CUA_APPKIT_MENU_POPOVER=1`). Both are the shapes a
/// window-scoped observation and a window capture have to account for: the
/// menu is an accessory window outside the window's subtree, the popover is
/// a same-process window the capture is drawn with.
let kPopupMenuAID = "pop-menu"
let kPopupChoiceAID = "lbl-popup-choice"
let kPopupOptionTitles = ["None", "5 minutes before", "15 minutes before"]
let kPopoverButtonAID = "btn-popover"
let kPopoverBodyAID = "lbl-popover-body"
let kPopoverBodyText = "POPOVER_BODY_MARKER_v1"
/// Taller than the distance from the button to the window's bottom edge, so
/// AppKit places it overflowing the window rather than inside it.
let kPopoverContentSize = NSSize(width: 320, height: 460)
/// How far inside the window's trailing edge the popover is anchored (points).
let kPopoverOverlap: CGFloat = 40
/// child_editor — the inline-editor shape (`CUA_APPKIT_CHILD_EDITOR=1`, or
/// `=sheet` to also open an attached sheet at launch). The application edits
/// in a separate borderless WindowServer window drawn inside the window it
/// edits, and accessibility publishes that window as the text field itself,
/// not as a window. See [`ChildEditorWindow`] for the measured shape.
let kChildEditorFieldAID = "txt-child-editor"
let kChildEditorStateAID = "lbl-child-editor"
let kChildEditorValueAID = "lbl-child-editor-value"
let kChildEditorCommitAID = "lbl-child-editor-commit"
let kChildEditorSeedValue = "child editor seed"
// Not a superstring of the harness window's title: the harness resolves its
// window by title substring, front first, and the sheet is front when open.
let kChildEditorSheetTitle = "Child Editor Sheet (CuaTestHarness)"
/// Where the editor sits relative to the main window's own origin, and how
/// big it is. Both keep it strictly inside the window's frame, which is what
/// makes the window its owner rather than a sibling's.
let kChildEditorOffset = NSPoint(x: 120, y: 200)
let kChildEditorSize = NSSize(width: 220, height: 24)

// MARK: - Controls

/// An `NSTextField` whose `AXValue` catches up with the text it holds only
/// after `CUA_APPKIT_AX_VALUE_LAG_MS`, growing a character at a time.
///
/// AppKit rebuilds a field's editor around an insertion, so the value read
/// microseconds after an `AXSelectedText` write is a prefix of what landed:
/// measured in Contacts, "(408) " of "(408) 961-1560", complete ~20 ms later.
/// Unset, the field behaves like a stock `NSTextField`.
final class LaggingTextField: NSTextField {
    private var reported = ""
    private var changedAt: Date?

    override func accessibilityValue() -> String? {
        let actual = stringValue
        let lag = HarnessWindowController.envSeconds("CUA_APPKIT_AX_VALUE_LAG_MS")
        guard lag > 0 else { return actual }
        if actual != reported {
            reported = actual
            changedAt = Date()
        }
        guard let changedAt, !actual.isEmpty else { return actual }
        let elapsed = Date().timeIntervalSince(changedAt)
        if elapsed >= lag { return actual }
        let visible = max(1, Int(Double(actual.count) * elapsed / lag))
        return String(actual.prefix(visible))
    }
}

/// An `NSTextField` whose advertised `AXConfirm` *is* its commit: performing
/// the action publishes the value the app took, the way a search field runs
/// its search. A control that advertises the action and acts on it is the one
/// case where an `AXValue` write plus `AXConfirm` is the application's own
/// end-of-edit — which is why the driver picks that route for it and types
/// into a plain bound field instead, whatever that one advertises.
final class ConfirmCommitTextField: NSTextField {
    var onConfirm: (String) -> Void = { _ in }

    override func accessibilitySubrole() -> NSAccessibility.Subrole? { .searchField }

    override func accessibilityActionNames() -> [NSAccessibility.Action] { [.confirm] }

    override func accessibilityPerformConfirm() -> Bool {
        onConfirm(stringValue)
        return true
    }
}

/// The Reminders reminder-row shape: a row that advertises `AXPress` (the app's
/// own default action, `row_pressed=`) and exposes a settable `AXSelected`
/// (what a pointer click does, `row_selected=`), so the two are separable.
final class PressableRowView: NSView {
    var onPress: () -> Void = {}
    var onSelect: () -> Void = {}
    private var selected = false

    override func draw(_ dirtyRect: NSRect) {
        let fill = selected ? NSColor.selectedContentBackgroundColor : NSColor.controlBackgroundColor
        fill.setFill()
        dirtyRect.fill()
    }

    override func mouseDown(with event: NSEvent) {
        setAccessibilitySelected(true)
    }

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .row }

    override func accessibilityLabel() -> String? { "pressable row" }

    override func accessibilityActionNames() -> [NSAccessibility.Action] { [.press] }

    override func isAccessibilitySelected() -> Bool { selected }

    override func setAccessibilitySelected(_ accessibilitySelected: Bool) {
        guard selected != accessibilitySelected else { return }
        selected = accessibilitySelected
        needsDisplay = true
        onSelect()
    }

    override func accessibilityPerformPress() -> Bool {
        onPress()
        return true
    }
}

/// The Reminders row-group shape: an `AXGroup` that carries the application's
/// own `AXHelp` explaining what its press does, and advertises that press.
final class HelpfulGroupView: NSView {
    var onPress: () -> Void = {}

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .group }

    override func accessibilityLabel() -> String? { "row detail" }

    override func accessibilityHelp() -> String? {
        "To mark the row as done, press Control-Option-Space."
    }

    override func accessibilityActionNames() -> [NSAccessibility.Action] { [.press] }

    override func accessibilityPerformPress() -> Bool {
        onPress()
        return true
    }
}

/// The Finder/Mail/Notes row-name-cell shape: a row that *looks* like a text
/// field — role `AXTextField`, a value, an advertised action — and is not an
/// editor. It never becomes first responder, so the `AXFocused` write is
/// accepted and changes nothing, and keystrokes aimed at it land on whatever
/// the window's real first responder is. A rename gesture is what opens an
/// editor over such a row; the row itself never holds one.
final class RowNameCellField: NSTextField {
    override var acceptsFirstResponder: Bool { false }

    override func accessibilityRole() -> NSAccessibility.Role? { .textField }

    override func accessibilityActionNames() -> [NSAccessibility.Action] { [.confirm] }

    override func accessibilityPerformConfirm() -> Bool { true }

    override func isAccessibilityFocused() -> Bool { false }

    override func setAccessibilityFocused(_ accessibilityFocused: Bool) {}
}

/// A window that publishes its own first responder.
///
/// AppKit posts no first-responder-changed notification and the driver's tree
/// carries no `AXFocused` column, so which control holds keyboard focus is
/// only assertable if the application says it.
final class HarnessWindow: NSWindow {
    var onFirstResponderChange: (() -> Void)?

    @discardableResult
    override func makeFirstResponder(_ responder: NSResponder?) -> Bool {
        let accepted = super.makeFirstResponder(responder)
        onFirstResponderChange?()
        return accepted
    }
}

/// The inline-editor shape: a borderless child window that accessibility
/// publishes as the text field it contains.
///
/// Measured in Finder's inline rename editor, which is what this reproduces:
/// the editor is its own layer-0 WindowServer window drawn inside the window
/// being renamed, and the application answers `AXWindows` with the editor's
/// `AXTextField` — role `AXTextField`, `CFEqual` to the application's
/// `AXFocusedUIElement`, mapped by `_AXUIElementGetWindow` to the child's
/// CGWindowID, with no `AXWindow` and no `AXTopLevelUIElement` attribute and
/// an `AXParent` chain that goes straight to `AXApplication`. While it is up
/// the application answers no `AXFocusedWindow` at all.
///
/// Nothing can address, activate or make such a surface key on its own, and
/// making its parent window key is exactly what dismisses it — so a driver
/// that fronts the parent to deliver a keystroke destroys the destination it
/// was aiming at. `canBecomeKey` is overridden because a borderless window
/// refuses key status by default; the real editors this stands in for take it.
final class ChildEditorWindow: NSWindow {
    let field = NSTextField(string: kChildEditorSeedValue)

    override var canBecomeKey: Bool { true }

    override func isAccessibilityElement() -> Bool { true }

    override func accessibilityRole() -> NSAccessibility.Role? { .textField }

    override func accessibilityValue() -> Any? { field.stringValue }

    override func accessibilityTitle() -> String? { nil }

    override func accessibilityChildren() -> [Any]? { nil }

    override func isAccessibilityFocused() -> Bool { isKeyWindow }


    /// An `AXFocused` write is how a driver asks for the keyboard without a
    /// click; the editor answers by taking key and putting its field first.
    override func setAccessibilityFocused(_ focused: Bool) {
        guard focused else { return }
        makeKeyAndOrderFront(nil)
        if field.currentEditor() == nil {
            makeFirstResponder(field)
        }
    }


    override func setAccessibilityValue(_ value: Any?) {
        guard let text = value as? String else { return }
        field.stringValue = text
        if let editor = field.currentEditor() {
            editor.string = text
        }
        onValueChanged?()
    }

    /// Set by the owning surface so a value written through AX (which raises
    /// no `controlTextDidChange`) still reaches the mirrors.
    var onValueChanged: (() -> Void)?
}

/// Owns the child editor and mirrors its state into the main window.
///
/// The mirrors are the point: the editor is not in the main window's AX
/// subtree, so a window-scoped observation of the main window can only learn
/// what landed in the editor if the application says it there.
final class ChildEditorSurface: NSObject, NSTextFieldDelegate {
    let editor = ChildEditorWindow(
        contentRect: NSRect(origin: .zero, size: kChildEditorSize),
        styleMask: [.borderless], backing: .buffered, defer: false)
    let stateLabel = NSTextField(labelWithString: "child_editor=closed")
    let valueLabel = NSTextField(labelWithString: "child_value=none")
    let commitLabel = NSTextField(labelWithString: "child_committed=none")
    private let parent: NSWindow
    private let withSheet: Bool
    private var sheet: NSWindow?

    init(parent: NSWindow, withSheet: Bool) {
        self.parent = parent
        self.withSheet = withSheet
        super.init()
        stateLabel.setAccessibilityIdentifier(kChildEditorStateAID)
        valueLabel.setAccessibilityIdentifier(kChildEditorValueAID)
        commitLabel.setAccessibilityIdentifier(kChildEditorCommitAID)
        editor.setAccessibilityIdentifier(kChildEditorFieldAID)
        editor.isReleasedWhenClosed = false
        editor.isRestorable = false
        editor.hasShadow = false
        let content = NSView(frame: NSRect(origin: .zero, size: kChildEditorSize))
        editor.field.frame = content.bounds
        editor.field.delegate = self
        editor.field.target = self
        editor.field.action = #selector(onCommit)
        // Return commits (the action). Losing first responder must not: a
        // text field's cell fires its action on end-of-editing by default,
        // which committed and closed the editor the moment anything else took
        // key — the sheet below, or the harness backgrounding the process.
        // Finder's editor does close on deactivation; the fixture keeps its
        // editor up through it so the background rungs can be exercised at
        // all, and dismisses on the trigger that was measured to matter: the
        // parent window becoming key (see `parentBecameKey`).
        editor.field.cell?.sendsActionOnEndEditing = false
        editor.onValueChanged = { [weak self] in self?.publish() }
        content.addSubview(editor.field)
        editor.contentView = content
        NotificationCenter.default.addObserver(
            self, selector: #selector(parentBecameKey(_:)),
            name: NSWindow.didBecomeKeyNotification, object: parent)
    }

    /// Making the parent key is what dismisses an inline editor — measured in
    /// Finder, one foreground chord at the parent ended the rename outright.
    /// A driver that fronts the parent to reach the editor destroys it; the
    /// mirrors then read `closed`, which is what the harness asserts against.
    @objc private func parentBecameKey(_ notification: Notification) {
        guard editor.isVisible else { return }
        onCommit()
    }

    /// Opens the editor inside the parent's current frame. Called after the
    /// main window has been centered, so the frame it is placed in is final.
    func open() {
        editor.setFrameOrigin(NSPoint(
            x: parent.frame.origin.x + kChildEditorOffset.x,
            y: parent.frame.origin.y + kChildEditorOffset.y))
        parent.addChildWindow(editor, ordered: .above)
        editor.makeKeyAndOrderFront(nil)
        editor.makeFirstResponder(editor.field)
        editor.field.currentEditor()?.selectAll(nil)
        publish()
        if withSheet {
            openSheet()
        }
    }

    /// An attached sheet, for the contrast: a sheet is published as an
    /// `AXSheet` child of its parent window (not in the application's
    /// `AXWindows`), becomes its application's key window, and is therefore a
    /// keyboard destination of its own — unlike the editor above.
    func openSheet() {
        let candidate = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 360, height: 160),
            styleMask: [.titled], backing: .buffered, defer: false)
        candidate.title = kChildEditorSheetTitle
        candidate.contentView = NSTextField(labelWithString: "attached sheet takes key")
        sheet = candidate
        parent.beginSheet(candidate)
    }

    func publish() {
        stateLabel.stringValue = editor.isVisible
            ? "child_editor=open id=\(editor.windowNumber)"
            : "child_editor=closed"
        valueLabel.stringValue = "child_value=\(editor.field.stringValue)"
    }

    func controlTextDidChange(_ notification: Notification) {
        publish()
    }

    /// Return commits and dismisses the editor, the way an inline rename does.
    @objc private func onCommit() {
        commitLabel.stringValue = "child_committed=\(editor.field.stringValue)"
        parent.removeChildWindow(editor)
        editor.orderOut(nil)
        parent.makeKeyAndOrderFront(nil)
        publish()
    }
}

// MARK: - Controller

final class HarnessWindowController: NSObject, NSTextFieldDelegate, NSTableViewDataSource, NSTableViewDelegate, NSMenuItemValidation {
    let window: HarnessWindow
    let counterLabel = NSTextField(labelWithString: "counter=0")
    var counterValue = 0
    let textInput = LaggingTextField(string: "")
    let textInputMirror = NSTextField(labelWithString: "")
    let textInputCommit = NSTextField(labelWithString: "committed=none")
    let reformatInput = NSTextField(string: "")
    var swapInput = NSTextField(string: "")
    let discardInput = NSTextField(string: kDiscardKeptValue)
    let confirmInput = ConfirmCommitTextField(string: "")
    let nonBmpInput = NSTextField(string: kNonBmpSeedValue)
    let endOfEditRow = NSStackView()
    let endOfEditLabel = NSTextField(labelWithString: "")
    var reformatCommitted = "none"
    var swapCommitted = "none"
    var swaps = 0
    var discardCommitted = "none"
    var confirmCommitted = "none"
    var confirmTyped = 0
    var nonBmpCommitted = "none"
    let lastActionLabel = NSTextField(labelWithString: "last_action=none")
    let clickCountLabel = NSTextField(labelWithString: "clicks=0")
    var clicks = 0
    let sliderValueLabel = NSTextField(labelWithString: "slider_value=0")
    let checkStateLabel = NSTextField(labelWithString: "agreed=false")
    let selectionItems = ["alpha", "beta", "gamma"]
    let selectionTable = NSTableView()
    let selectionStateLabel = NSTextField(labelWithString: "selection=none")
    let pressableRow = PressableRowView()
    let pressableRowStateLabel = NSTextField(
        labelWithString: "row_pressed=0 row_selected=false group_pressed=0")
    var pressableRowPresses = 0
    let rowHelpGroup = HelpfulGroupView()
    var rowHelpGroupPresses = 0
    let menuActionLabel = NSTextField(labelWithString: "menu_action=none")
    // Arrange ▸ Left is the one menu command a cell fires twice (once per
    // delivery rung), and an idempotent label makes the second firing
    // invisible — both to the assertion and to the driver's own change probe,
    // which then reports the landed chord as `suspected_noop`. Count it.
    var arrangeLeftFirings = 0
    let scrollOffsetLabel = NSTextField(labelWithString: "scroll_offset=0")
    let accelCountLabel = NSTextField(labelWithString: "accel_fired=0")
    var accelCount = 0
    var keyMonitor: Any?
    let rowNameCell = RowNameCellField(string: kRowNameCellValue)
    let firstResponderLabel = NSTextField(labelWithString: "first_responder=none")
    let popupButton = NSPopUpButton()
    let popupChoiceLabel = NSTextField(labelWithString: "popup_choice=none")
    let popoverButton = NSButton(title: "Show popover", target: nil, action: nil)
    let popoverBodyLabel = NSTextField(labelWithString: kPopoverBodyText)
    /// Retained for the window's life: an `NSPopover` that goes away takes
    /// its window with it, and the capture geometry this fixture exists to
    /// exercise is only there while the popover is on screen.
    let popover = NSPopover()
    /// The inline-editor scenario's child window and its mirrors. Opened
    /// after [`show`] has centered the main window, so it is placed inside
    /// the frame the window actually ends up with.
    var childEditor: ChildEditorSurface?

    // Pinned content size — every launch MUST produce a byte-identical window
    // so screenshot dimensions (and the hardcoded pixel coords the harness tests
    // rely on) never drift.
    static let kContentSize = NSSize(width: 720, height: 860)

    override init() {
        let rect = NSRect(origin: NSPoint(x: 100, y: 100), size: HarnessWindowController.kContentSize)
        // No `.resizable`: a resizable window can be left at a different size,
        // and macOS would persist/restore that drifted frame on the next launch.
        let mask: NSWindow.StyleMask = [.titled, .closable, .miniaturizable]
        window = HarnessWindow(contentRect: rect, styleMask: mask, backing: .buffered, defer: false)
        window.title = kWindowTitle
        window.setAccessibilityIdentifier(kWindowAID)
        window.isReleasedWhenClosed = false
        // Deterministic geometry across launches. macOS persists and restores a
        // window's frame by default (Cocoa state restoration + frame autosave),
        // so a window that was nudged/resized — or laid out a hair differently on
        // a prior run — reopens at a drifted height (observed 832 vs 858 pt),
        // shifting screenshot dimensions and breaking tests that assume fixed
        // pixel coords. Opt out of restoration entirely and re-pin the content
        // size on every launch so each run is identical.
        window.isRestorable = false
        window.setFrameAutosaveName("")
        window.setContentSize(HarnessWindowController.kContentSize)
        super.init()
        buildContent()
        installKeyboardMonitor()
        installRememberedResponder()
    }

    func show() {
        window.makeKeyAndOrderFront(nil)
        window.center()
        // `show` runs before `app.run()`, so the parent is only ordered in when
        // the application finishes launching. An editor opened before that
        // loses key status to the parent's deferred ordering, ends its edit
        // session and dismisses itself — the inline-editor behaviour the
        // scenario exists to exercise, fired by the launch instead of a test.
        // Open it once the parent is really on screen and key.
        if let editor = childEditor {
            NotificationCenter.default.addObserver(
                forName: NSApplication.didFinishLaunchingNotification, object: nil, queue: .main
            ) { _ in
                editor.open()
            }
        }
    }

    // MARK: - Layout

    private func buildContent() {
        let content = NSStackView()
        content.orientation = .vertical
        content.alignment = .leading
        content.spacing = 8
        content.edgeInsets = NSEdgeInsets(top: 12, left: 20, bottom: 12, right: 20)
        content.translatesAutoresizingMaskIntoConstraints = false

        // counter
        content.addArrangedSubview(sectionLabel("counter"))
        let counterRow = NSStackView()
        counterRow.orientation = .horizontal
        counterRow.spacing = 12
        let inc = NSButton(title: "Increment", target: self, action: #selector(onIncrement))
        inc.setAccessibilityIdentifier(kIncrementButtonAID)
        let reset = NSButton(title: "Reset", target: self, action: #selector(onReset))
        reset.setAccessibilityIdentifier(kResetButtonAID)
        counterLabel.setAccessibilityIdentifier(kCounterLabelAID)
        counterLabel.font = NSFont.monospacedSystemFont(ofSize: 18, weight: .semibold)
        counterRow.addArrangedSubview(inc)
        counterRow.addArrangedSubview(reset)
        counterRow.addArrangedSubview(counterLabel)
        content.addArrangedSubview(counterRow)

        // text_body
        content.addArrangedSubview(sectionLabel("text_body"))
        let body = NSTextField(labelWithString:
            "This is the body of the harness test app. Marker: \(kTextBodyMarker). " +
            "Used to verify get_window_state can extract known text.")
        body.setAccessibilityIdentifier(kTextBodyAID)
        body.maximumNumberOfLines = 3
        body.preferredMaxLayoutWidth = 600
        content.addArrangedSubview(body)

        // text_input
        content.addArrangedSubview(sectionLabel("text_input"))
        textInput.setAccessibilityIdentifier(kTextInputAID)
        textInput.placeholderString = "Type here…"
        textInput.delegate = self
        textInput.translatesAutoresizingMaskIntoConstraints = false
        textInputMirror.setAccessibilityIdentifier(kTextInputMirrorAID)
        textInputMirror.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        textInputCommit.setAccessibilityIdentifier(kTextInputCommitAID)
        textInputCommit.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let inputRow = NSStackView()
        inputRow.orientation = .horizontal
        inputRow.spacing = 12
        inputRow.addArrangedSubview(textInput)
        inputRow.addArrangedSubview(textInputMirror)
        inputRow.addArrangedSubview(textInputCommit)
        NSLayoutConstraint.activate([
            textInput.widthAnchor.constraint(equalToConstant: 240),
        ])
        content.addArrangedSubview(inputRow)

        // click_target — a REAL NSButton so it is in the AX tree and addressable
        // by element_index (AppKit NSButton ignores synthetic pixel clicks, but
        // AXPress works). AXPress / single mouse → click; pixel double → double_click;
        // right-click → right_click. (matches WPF btn-clicktarget contract.)
        content.addArrangedSubview(sectionLabel("click_target"))
        let clickTarget = ClickTargetButton(title: "Click target (left / right / double)",
                                             target: self, action: #selector(onClickTarget))
        clickTarget.harness = self
        clickTarget.setAccessibilityIdentifier(kClickTargetAID)
        lastActionLabel.setAccessibilityIdentifier(kLastActionAID)
        lastActionLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        clickCountLabel.setAccessibilityIdentifier(kClickCountAID)
        clickCountLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        accelCountLabel.setAccessibilityIdentifier(kAccelCountAID)
        accelCountLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let clickRow = NSStackView()
        clickRow.orientation = .horizontal
        clickRow.spacing = 12
        clickRow.addArrangedSubview(clickTarget)
        clickRow.addArrangedSubview(lastActionLabel)
        clickRow.addArrangedSubview(clickCountLabel)
        clickRow.addArrangedSubview(accelCountLabel)
        content.addArrangedSubview(clickRow)

        // slider — NSSlider drives the `drag` / `set_value` tools (AXValue).
        content.addArrangedSubview(sectionLabel("slider"))
        let slider = NSSlider(value: 0, minValue: 0, maxValue: 100,
                              target: self, action: #selector(onSlider))
        slider.setAccessibilityIdentifier(kSliderAID)
        slider.isContinuous = true
        slider.translatesAutoresizingMaskIntoConstraints = false
        sliderValueLabel.setAccessibilityIdentifier(kSliderValueAID)
        sliderValueLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let sliderRow = NSStackView()
        sliderRow.orientation = .horizontal
        sliderRow.spacing = 12
        sliderRow.addArrangedSubview(slider)
        sliderRow.addArrangedSubview(sliderValueLabel)
        NSLayoutConstraint.activate([slider.widthAnchor.constraint(equalToConstant: 320)])
        content.addArrangedSubview(sliderRow)

        // checkable_controls — NSButton checkbox toggles AXValue.
        content.addArrangedSubview(sectionLabel("checkable_controls"))
        let checkbox = NSButton(checkboxWithTitle: "I agree",
                                target: self, action: #selector(onCheckbox(_:)))
        checkbox.setAccessibilityIdentifier(kCheckboxAID)
        checkbox.state = .off
        checkStateLabel.setAccessibilityIdentifier(kCheckStateAID)
        checkStateLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let checkRow = NSStackView()
        checkRow.orientation = .horizontal
        checkRow.spacing = 12
        checkRow.addArrangedSubview(checkbox)
        checkRow.addArrangedSubview(checkStateLabel)
        selectionTable.headerView = nil
        selectionTable.allowsMultipleSelection = true
        selectionTable.dataSource = self
        selectionTable.delegate = self
        let selectionColumn = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("selection-column"))
        selectionColumn.width = 140
        selectionTable.addTableColumn(selectionColumn)
        let selectionScroll = NSScrollView()
        selectionScroll.documentView = selectionTable
        selectionScroll.hasVerticalScroller = true
        selectionScroll.borderType = .lineBorder
        selectionScroll.translatesAutoresizingMaskIntoConstraints = false
        selectionStateLabel.setAccessibilityIdentifier(kSelectionStateAID)
        selectionStateLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        checkRow.addArrangedSubview(selectionScroll)
        checkRow.addArrangedSubview(selectionStateLabel)
        NSLayoutConstraint.activate([
            selectionScroll.widthAnchor.constraint(equalToConstant: 150),
            selectionScroll.heightAnchor.constraint(equalToConstant: 68),
        ])
        content.addArrangedSubview(checkRow)

        // context_menu — NSButton with an attached NSMenu. Right-click opens the
        // native contextual menu; selecting an item updates menu_action=.
        content.addArrangedSubview(sectionLabel("context_menu"))
        let contextButton = NSButton(title: "Right-click for context menu",
                                     target: nil, action: nil)
        contextButton.setAccessibilityIdentifier(kContextButtonAID)
        let ctxMenu = NSMenu()
        for title in ["Cut", "Copy", "Paste"] {
            let item = NSMenuItem(title: title, action: #selector(onContextItem(_:)), keyEquivalent: "")
            item.target = self
            item.setAccessibilityIdentifier("ctx-\(title.lowercased())")
            ctxMenu.addItem(item)
        }
        contextButton.menu = ctxMenu
        menuActionLabel.setAccessibilityIdentifier(kMenuActionAID)
        menuActionLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let contextRow = NSStackView()
        contextRow.orientation = .horizontal
        contextRow.spacing = 12
        contextRow.addArrangedSubview(contextButton)
        contextRow.addArrangedSubview(menuActionLabel)
        content.addArrangedSubview(contextRow)

        // scroll_target
        content.addArrangedSubview(sectionLabel("scroll_target"))
        let scrollWrap = NSStackView()
        scrollWrap.orientation = .horizontal
        scrollWrap.spacing = 12
        let scroller = NSScrollView(frame: NSRect(x: 0, y: 0, width: 480, height: 120))
        scroller.translatesAutoresizingMaskIntoConstraints = false
        scroller.hasVerticalScroller = true
        scroller.borderType = .lineBorder
        let bodyText = NSTextView(frame: NSRect(x: 0, y: 0, width: 460, height: 600))
        bodyText.isEditable = false
        // The NSScrollView's AXScrollArea is NOT surfaced by get_window_state — only
        // the document AXTextArea is. Put scroll-tall on the document view so the
        // scroller_aid contract resolves to the actual scrollable AX node.
        bodyText.setAccessibilityIdentifier(kScrollerAID)
        // Keep this small (one AX node per line on macOS) so the get_window_state
        // tree walk doesn't exhaust its element budget before reaching the rest
        // of the scenarios. 30 lines is plenty for verifying scroll offset.
        var bigBody = kScrollTopMarker + "\n"
        for i in 0..<30 {
            bigBody += "Line \(i)\n"
        }
        bigBody += kScrollBottomMarker
        bodyText.string = bigBody
        scroller.documentView = bodyText
        scrollOffsetLabel.setAccessibilityIdentifier(kScrollOffsetAID)
        scrollOffsetLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        NotificationCenter.default.addObserver(
            self, selector: #selector(onScroll),
            name: NSView.boundsDidChangeNotification,
            object: scroller.contentView)
        scroller.contentView.postsBoundsChangedNotifications = true
        scrollWrap.addArrangedSubview(scroller)
        scrollWrap.addArrangedSubview(scrollOffsetLabel)
        NSLayoutConstraint.activate([
            scroller.widthAnchor.constraint(equalToConstant: 480),
            scroller.heightAnchor.constraint(equalToConstant: 120),
        ])
        content.addArrangedSubview(scrollWrap)

        // exit
        let exit = NSButton(title: "Exit", target: self, action: #selector(onExit))
        exit.setAccessibilityIdentifier(kExitButtonAID)
        content.addArrangedSubview(exit)

        // selectable_row — appended last so no section above it shifts.
        content.addArrangedSubview(sectionLabel("selectable_row"))
        pressableRow.setAccessibilityIdentifier(kPressableRowAID)
        pressableRow.translatesAutoresizingMaskIntoConstraints = false
        pressableRow.onPress = { [weak self] in self?.onPressableRowPress() }
        pressableRow.onSelect = { [weak self] in self?.publishPressableRowState() }
        rowHelpGroup.setAccessibilityIdentifier(kRowHelpGroupAID)
        rowHelpGroup.translatesAutoresizingMaskIntoConstraints = false
        rowHelpGroup.onPress = { [weak self] in self?.onRowHelpGroupPress() }
        pressableRowStateLabel.setAccessibilityIdentifier(kPressableRowStateAID)
        pressableRowStateLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let pressableRowStack = NSStackView()
        pressableRowStack.orientation = .horizontal
        pressableRowStack.spacing = 12
        pressableRowStack.addArrangedSubview(pressableRow)
        pressableRowStack.addArrangedSubview(rowHelpGroup)
        pressableRowStack.addArrangedSubview(pressableRowStateLabel)
        NSLayoutConstraint.activate([
            pressableRow.widthAnchor.constraint(equalToConstant: 160),
            pressableRow.heightAnchor.constraint(equalToConstant: 28),
            rowHelpGroup.widthAnchor.constraint(equalToConstant: 60),
            rowHelpGroup.heightAnchor.constraint(equalToConstant: 28),
        ])
        content.addArrangedSubview(pressableRowStack)

        // end_of_edit — appended last so no section above it shifts. One
        // reformats the value it keeps, one replaces the control itself, one
        // discards the edit, one takes its commit from its own advertised
        // AXConfirm, and one starts out holding a non-BMP value a replacement
        // has to select past.
        configureEndOfEditField(
            reformatInput, aid: kReformatInputAID, placeholder: kReformatPlaceholder)
        configureEndOfEditField(swapInput, aid: kSwapInputAID, placeholder: kSwapPlaceholder)
        configureEndOfEditField(
            discardInput, aid: kDiscardInputAID, placeholder: kDiscardPlaceholder)
        configureEndOfEditField(
            confirmInput, aid: kConfirmInputAID, placeholder: kConfirmPlaceholder)
        configureEndOfEditField(nonBmpInput, aid: kNonBmpInputAID, placeholder: kNonBmpPlaceholder)
        confirmInput.onConfirm = { [weak self] value in
            guard let self else { return }
            self.confirmCommitted = value
            self.publishEndOfEdit()
        }
        endOfEditRow.orientation = .horizontal
        endOfEditRow.spacing = 8
        endOfEditRow.addArrangedSubview(reformatInput)
        endOfEditRow.addArrangedSubview(swapInput)
        endOfEditRow.addArrangedSubview(discardInput)
        endOfEditRow.addArrangedSubview(confirmInput)
        endOfEditRow.addArrangedSubview(nonBmpInput)
        endOfEditLabel.setAccessibilityIdentifier(kEndOfEditStateAID)
        endOfEditLabel.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        publishEndOfEdit()
        content.addArrangedSubview(endOfEditRow)
        content.addArrangedSubview(endOfEditLabel)

        // focus_target — appended last so no section above it shifts. A row
        // that looks like a text field and cannot take keyboard focus, beside
        // the window's own report of which control holds it.
        content.addArrangedSubview(sectionLabel("focus_target"))
        rowNameCell.setAccessibilityIdentifier(kRowNameCellAID)
        rowNameCell.isEditable = false
        rowNameCell.isSelectable = false
        rowNameCell.isBezeled = false
        rowNameCell.drawsBackground = false
        firstResponderLabel.setAccessibilityIdentifier(kFirstResponderAID)
        firstResponderLabel.font = NSFont.monospacedSystemFont(ofSize: 14, weight: .regular)
        let focusRow = NSStackView()
        focusRow.orientation = .horizontal
        focusRow.spacing = 12
        focusRow.addArrangedSubview(rowNameCell)
        focusRow.addArrangedSubview(firstResponderLabel)
        content.addArrangedSubview(focusRow)
        window.onFirstResponderChange = { [weak self] in self?.publishFirstResponder() }
        publishFirstResponder()
        var extraHeight: CGFloat = 0
        if HarnessWindowController.envFlag("CUA_APPKIT_MENU_POPOVER") {
            content.addArrangedSubview(sectionLabel("menu_popover"))
            popupButton.setAccessibilityIdentifier(kPopupMenuAID)
            popupButton.removeAllItems()
            popupButton.addItems(withTitles: kPopupOptionTitles)
            popupButton.target = self
            popupButton.action = #selector(onPopupChoice(_:))
            popupChoiceLabel.setAccessibilityIdentifier(kPopupChoiceAID)
            popupChoiceLabel.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
            popoverButton.setAccessibilityIdentifier(kPopoverButtonAID)
            popoverButton.target = self
            popoverButton.action = #selector(onShowPopover)
            let popoverRow = NSStackView()
            popoverRow.orientation = .horizontal
            popoverRow.spacing = 12
            popoverRow.addArrangedSubview(popupButton)
            popoverRow.addArrangedSubview(popupChoiceLabel)
            popoverRow.addArrangedSubview(popoverButton)
            content.addArrangedSubview(popoverRow)
            extraHeight += 60
        }
        if let mode = ProcessInfo.processInfo.environment["CUA_APPKIT_CHILD_EDITOR"],
           mode == "1" || mode == "sheet" {
            content.addArrangedSubview(sectionLabel("child_editor"))
            let surface = ChildEditorSurface(parent: window, withSheet: mode == "sheet")
            for mirror in [surface.stateLabel, surface.valueLabel, surface.commitLabel] {
                mirror.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
            }
            let editorRow = NSStackView()
            editorRow.orientation = .horizontal
            editorRow.spacing = 12
            editorRow.addArrangedSubview(surface.stateLabel)
            editorRow.addArrangedSubview(surface.valueLabel)
            editorRow.addArrangedSubview(surface.commitLabel)
            content.addArrangedSubview(editorRow)
            childEditor = surface
            extraHeight += 60
        }
        if extraHeight > 0 {
            window.setContentSize(NSSize(
                width: HarnessWindowController.kContentSize.width,
                height: HarnessWindowController.kContentSize.height + extraHeight))
        }

        // No outer scroll-view wrap: the content is sized to fit the window
        // so the only scrollable surface is the inner scroll_target NSScrollView.
        // Otherwise scroll events delivered at window-local coords get
        // consumed by the outer scroll view before reaching the inner one,
        // and `scroll` tool tests can't deterministically move the inner offset.
        let container = NSView(frame: window.contentLayoutRect)
        container.autoresizingMask = [.width, .height]
        content.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(content)
        NSLayoutConstraint.activate([
            content.topAnchor.constraint(equalTo: container.topAnchor),
            content.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            content.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            content.widthAnchor.constraint(equalToConstant: 700),
        ])
        window.contentView = container
    }

    private func sectionLabel(_ id: String) -> NSTextField {
        let f = NSTextField(labelWithString: "▸ \(id)")
        f.font = NSFont.systemFont(ofSize: 13, weight: .bold)
        f.textColor = .secondaryLabelColor
        return f
    }

    /// The control the window's first responder belongs to, as
    /// `first_responder=<accessibility identifier>`.
    ///
    /// AppKit makes a field editor (an `NSTextView` with `isFieldEditor`) the
    /// first responder while a text field is being edited, and that editor is
    /// not the control an agent addressed — so it resolves to its client,
    /// which is the field itself.
    private func publishFirstResponder() {
        var responder = window.firstResponder
        if let editor = responder as? NSTextView, editor.isFieldEditor {
            responder = editor.delegate as? NSResponder ?? responder
        }
        let name: String
        if let view = responder as? NSView {
            let identifier = view.accessibilityIdentifier()
            name = identifier.isEmpty ? String(describing: type(of: view)) : identifier
        } else {
            name = "none"
        }
        firstResponderLabel.stringValue = "first_responder=\(name)"
    }

    private func installKeyboardMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) {
            [weak self] event in
            guard let self else { return event }
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            let chordFlags: NSEvent.ModifierFlags = [.control, .shift]
            let key = event.charactersIgnoringModifiers?.lowercased()
            let isChord = flags.contains(chordFlags) && key == "k"
            let hasModifiers = !flags.intersection([.command, .control, .option, .shift]).isEmpty
            let isPlainF5 = event.keyCode == 96 && !hasModifiers
            if isChord || isPlainF5 {
                self.accelCount += 1
                self.accelCountLabel.stringValue = "accel_fired=\(self.accelCount)"
                return nil
            }
            return event
        }
    }

    /// With `CUA_APPKIT_REMEMBERED_RESPONDER=1`, the window installs the
    /// selection table as its first responder every time it becomes key —
    /// the AppKit shape where a window remembers a responder that is not the
    /// text field an agent focused. Notes' note list does exactly this, and a
    /// window-scoped `type_text` that re-queries focus after the activation
    /// types into the table instead of the field.
    private func installRememberedResponder() {
        guard ProcessInfo.processInfo.environment["CUA_APPKIT_REMEMBERED_RESPONDER"] == "1" else {
            return
        }
        NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification,
            object: window,
            queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            self.window.makeFirstResponder(self.selectionTable)
        }
    }


    @objc private func onPopupChoice(_ sender: NSPopUpButton) {
        popupChoiceLabel.stringValue = "popup_choice=\(sender.titleOfSelectedItem ?? "none")"
    }

    /// Show a popover anchored at the window's trailing edge, so it is drawn
    /// past the window's right edge.
    ///
    /// A popover is a window of this process that the window server draws
    /// with the window it is anchored to, so a capture of the window covers
    /// both — and the rect those pixels cover is the union, not the window's
    /// own frame. The scenario only tests something while that union differs
    /// from the frame, so the popover must leave the window: anchored to its
    /// button it opens inside the frame (above it — `.maxY` is the top edge of
    /// an unflipped view), and below the window there is no screen left, so
    /// AppKit would fold it back in. `.semitransient` keeps it up while the
    /// application is in the background, which is when the driver observes it.
    @objc private func onShowPopover() {
        guard !popover.isShown else { return }
        popoverBodyLabel.setAccessibilityIdentifier(kPopoverBodyAID)
        popoverBodyLabel.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        let body = NSViewController()
        let container = NSView(frame: NSRect(origin: .zero, size: kPopoverContentSize))
        popoverBodyLabel.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(popoverBodyLabel)
        NSLayoutConstraint.activate([
            popoverBodyLabel.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            popoverBodyLabel.bottomAnchor.constraint(
                equalTo: container.bottomAnchor, constant: -16),
        ])
        body.view = container
        popover.contentViewController = body
        popover.contentSize = kPopoverContentSize
        popover.behavior = .semitransient
        // Anchored a little inside the edge: a popover opens flush against
        // its anchor rect's far side, and a surface that merely touches the
        // window is not over it — the overlap is what a real popover has by
        // way of the control it hangs from.
        let content = window.contentView!
        let anchor = content.convert(popoverButton.bounds, from: popoverButton)
        let trailing = NSRect(
            x: content.bounds.maxX - kPopoverOverlap, y: anchor.midY, width: 1, height: 1)
        popover.show(relativeTo: trailing, of: content, preferredEdge: .maxX)
    }

    deinit {
        if let keyMonitor {
            NSEvent.removeMonitor(keyMonitor)
        }
    }

    // MARK: - Actions

    @objc private func onIncrement() {
        // Real applications answer an AX action on their own main loop and
        // publish the result later: Contacts' toolbar add button opens its
        // menu ~1.3 s after AXPress returns. CUA_APPKIT_PRESS_LATENCY_MS
        // reproduces that so a driver that samples the target once, right
        // after the dispatch, is caught calling a real effect a no-op.
        let latency = HarnessWindowController.envSeconds("CUA_APPKIT_PRESS_LATENCY_MS")
        counterValue += 1
        let published = "counter=\(counterValue)"
        guard latency > 0 else {
            counterLabel.stringValue = published
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + latency) { [weak counterLabel] in
            counterLabel?.stringValue = published
        }
    }

    /// Whether an opt-in surface was asked for. Only `"1"` turns one on, so a
    /// stray empty value never changes a launch.
    static func envFlag(_ name: String) -> Bool {
        ProcessInfo.processInfo.environment[name] == "1"
    }

    /// Milliseconds from `name`, as seconds. Absent or unparseable is 0.
    static func envSeconds(_ name: String) -> TimeInterval {
        guard let raw = ProcessInfo.processInfo.environment[name],
              let ms = Double(raw), ms > 0 else { return 0 }
        return ms / 1000
    }

    @objc private func onReset() {
        counterValue = 0
        counterLabel.stringValue = "counter=0"
    }

    @objc private func onExit() {
        NSApp.terminate(nil)
    }

    @objc private func onScroll(_ note: Notification) {
        guard let clip = note.object as? NSClipView else { return }
        scrollOffsetLabel.stringValue = "scroll_offset=\(Int(clip.documentVisibleRect.origin.y))"
    }

    @objc private func onSlider(_ sender: NSSlider) {
        sliderValueLabel.stringValue = "slider_value=\(Int(sender.doubleValue.rounded()))"
    }

    @objc private func onCheckbox(_ sender: NSButton) {
        checkStateLabel.stringValue = "agreed=\(sender.state == .on)"
    }

    private func onPressableRowPress() {
        pressableRowPresses += 1
        publishPressableRowState()
    }

    private func onRowHelpGroupPress() {
        rowHelpGroupPresses += 1
        publishPressableRowState()
    }

    private func publishPressableRowState() {
        pressableRowStateLabel.stringValue =
            "row_pressed=\(pressableRowPresses) "
            + "row_selected=\(pressableRow.isAccessibilitySelected()) "
            + "group_pressed=\(rowHelpGroupPresses)"
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        selectionItems.count
    }

    func tableView(_ tableView: NSTableView,
                   viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let value = selectionItems[row]
        let field = NSTextField(labelWithString: value)
        field.isSelectable = true
        field.setAccessibilityIdentifier("selection-\(value)")
        return field
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        let values = selectionTable.selectedRowIndexes.map { selectionItems[$0] }
        selectionStateLabel.stringValue = values.isEmpty
            ? "selection=none"
            : "selection=\(values.joined(separator: ","))"
    }

    @objc private func onContextItem(_ sender: NSMenuItem) {
        menuActionLabel.stringValue = "menu_action=\(sender.title)"
    }

    @objc func onArrangeLeft(_ sender: NSMenuItem) {
        arrangeLeftFirings += 1
        menuActionLabel.stringValue = "menu_action=window_arrange_left#\(arrangeLeftFirings)"
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(onArrangeLeft(_:)) {
            // Real macOS Window-menu commands are contextual: the application
            // being active is insufficient when the requested window is not
            // key. Keep this fixture honest so invoke_menu must establish the
            // exact window context before resolving the final item.
            return NSApp.isActive && window.isKeyWindow
        }
        return true
    }

    func controlTextDidChange(_ obj: Notification) {
        guard let field = obj.object as? NSTextField else { return }
        if field === textInput {
            textInputMirror.stringValue = field.stringValue
        }
        if field === confirmInput {
            // Keystrokes reach a field editor and raise this notification; an
            // `AXValue` write does not. Counting it is how a test separates the
            // driver's typed route from its value-then-confirm route.
            confirmTyped += 1
            publishEndOfEdit()
        }
    }

    func controlTextDidEndEditing(_ obj: Notification) {
        guard let field = obj.object as? NSTextField else { return }
        if field === textInput {
            textInputCommit.stringValue = "committed=\(field.stringValue)"
            runControlledCommand(field.stringValue)
        }
        if field === reformatInput {
            // An end-of-edit formatter is ordinary AppKit: the app keeps the
            // edit and rewrites it, so the control ends up holding neither what
            // was written nor what it held before.
            let formatted = "[\(field.stringValue)]"
            field.stringValue = formatted
            reformatCommitted = formatted
            publishEndOfEdit()
        }
        if field === discardInput {
            // The other app: it refuses the edit and puts its own value back.
            field.stringValue = kDiscardKeptValue
            discardCommitted = kDiscardKeptValue
            publishEndOfEdit()
        }
        if field === nonBmpInput {
            nonBmpCommitted = field.stringValue
            publishEndOfEdit()
        }
        if field === swapInput {
            swapCommitted = field.stringValue
            swaps += 1
            publishEndOfEdit()
            replaceSwapField(field)
        }
    }

    /// Contacts' card editor replaces the control when the edit session ends:
    /// the `AXUIElementRef` a caller addressed goes invalid while a fresh
    /// instance holds the committed value — same parent, same place in it, same
    /// role and label. Done on the next pass of the run loop because the field
    /// editor this notification belongs to is still being torn down.
    private func replaceSwapField(_ field: NSTextField) {
        let committed = field.stringValue
        DispatchQueue.main.async { [weak self] in
            guard let self, self.swapInput === field else { return }
            let index = self.endOfEditRow.arrangedSubviews.firstIndex(of: field)
                ?? self.endOfEditRow.arrangedSubviews.count
            self.endOfEditRow.removeView(field)
            let replacement = NSTextField(string: committed)
            self.configureEndOfEditField(
                replacement, aid: kSwapInputAID, placeholder: kSwapPlaceholder)
            self.endOfEditRow.insertArrangedSubview(replacement, at: index)
            self.swapInput = replacement
        }
    }

    private func configureEndOfEditField(_ field: NSTextField, aid: String, placeholder: String) {
        field.setAccessibilityIdentifier(aid)
        field.placeholderString = placeholder
        field.delegate = self
        field.font = NSFont.monospacedSystemFont(ofSize: 12, weight: .regular)
        field.translatesAutoresizingMaskIntoConstraints = false
        field.widthAnchor.constraint(equalToConstant: 96).isActive = true
    }

    private func publishEndOfEdit() {
        endOfEditLabel.stringValue = [
            "reformat_committed=\(reformatCommitted)",
            "swap_committed=\(swapCommitted) swaps=\(swaps)",
            "discard_committed=\(discardCommitted)",
            "confirm_committed=\(confirmCommitted) confirm_typed=\(confirmTyped)",
            "nonbmp_committed=\(nonBmpCommitted)",
        ].joined(separator: " | ")
    }

    /// Test-only terminal-like command seam. It accepts exactly one harmless
    /// synthetic command and has a child process create the external oracle;
    /// arbitrary field contents are never executed.
    private func runControlledCommand(_ command: String) {
        guard command == "printf cua-press-key",
              let oraclePath = ProcessInfo.processInfo.environment["CUA_APPKIT_COMMAND_ORACLE"]
        else { return }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = [
            "-c",
            "printf cua-press-key > \"$1\"",
            "cua-appkit-command",
            oraclePath,
        ]
        do {
            try process.run()
        } catch {
            textInputCommit.stringValue = "command_error=launch_failed"
        }
    }

    // Click target — single source of truth for all three actions.
    @objc func onClickTarget() { recordClick("click") }
    func clickTargetSawDouble() { recordClick("double_click") }
    func clickTargetSawRight() { recordClick("right_click") }
    private func recordClick(_ action: String) {
        clicks += 1
        lastActionLabel.stringValue = "last_action=\(action)"
        clickCountLabel.stringValue = "clicks=\(clicks)"
    }
}

// MARK: - Click target button

// A real NSButton (so it shows up in the AX tree and is element_index-addressable
// via AXPress) that additionally reports double-click and right-click. AXPress and
// single mouse-up fire the target/action (→ click); a pixel double-click is caught
// here before super so it reports double_click; right-click reports right_click.
final class ClickTargetButton: NSButton {
    weak var harness: HarnessWindowController?

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 {
            harness?.clickTargetSawDouble()
            return
        }
        super.mouseDown(with: event)
    }
    override func rightMouseDown(with event: NSEvent) {
        harness?.clickTargetSawRight()
    }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

// Opt-in windows for the persistent exact-window activation certification.
// Ordinary harness launches remain byte-for-byte and behaviorally unchanged.
final class BringToFrontMatrixWindows {
    let secondary: NSWindow
    var sheet: NSWindow?
    var floating: NSPanel?

    init(parent: NSWindow, mode: String) {
        secondary = NSWindow(
            contentRect: NSRect(x: 40, y: 40, width: 420, height: 240),
            styleMask: [.titled, .closable], backing: .buffered, defer: false)
        secondary.title = kSecondaryWindowTitle
        secondary.isReleasedWhenClosed = false
        secondary.isRestorable = false
        secondary.contentView = NSTextField(labelWithString: "bring_to_front secondary ordinary window")
        secondary.orderFront(nil)

        if mode == "sheet" {
            let candidate = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 360, height: 160),
                styleMask: [.titled], backing: .buffered, defer: false)
            candidate.title = kSheetWindowTitle
            candidate.contentView = NSTextField(labelWithString: "modal sheet blocks parent key status")
            sheet = candidate
            parent.beginSheet(candidate)
        } else if mode == "floating" {
            let candidate = NSPanel(
                contentRect: NSRect(x: 180, y: 180, width: 320, height: 140),
                styleMask: [.titled, .utilityWindow], backing: .buffered, defer: false)
            candidate.title = kFloatingWindowTitle
            candidate.level = .floating
            candidate.isFloatingPanel = true
            candidate.contentView = NSTextField(labelWithString: "floating accessory panel")
            candidate.orderFront(nil)
            floating = candidate
        }
    }
}

func writeBringToFrontWindowReport(
    main: NSWindow,
    matrix: BringToFrontMatrixWindows?
) {
    guard let path = ProcessInfo.processInfo.environment["CUA_HARNESS_WINDOW_REPORT"] else {
        return
    }
    var lines = ["main=\(main.windowNumber)"]
    if let matrix {
        lines.append("secondary=\(matrix.secondary.windowNumber)")
        if let sheet = matrix.sheet {
            lines.append("sheet=\(sheet.windowNumber)")
        }
        if let floating = matrix.floating {
            lines.append("floating=\(floating.windowNumber)")
        }
    }
    do {
        try (lines.joined(separator: "\n") + "\n").write(
            toFile: path, atomically: true, encoding: .utf8)
    } catch {
        fputs("failed to write window report: \(error)\n", stderr)
    }
}

// MARK: - Menu bar (Mac-specific scenario: ns_menubar)

func installMenuBar(target: HarnessWindowController) {
    let main = NSMenu()
    let appItem = NSMenuItem()
    main.addItem(appItem)
    let appMenu = NSMenu(title: "App")
    let testItem = NSMenuItem(title: kMenuItemTitle, action: nil, keyEquivalent: "")
    testItem.setAccessibilityIdentifier("menu-test-item")
    appMenu.addItem(testItem)
    appMenu.addItem(NSMenuItem.separator())
    appMenu.addItem(NSMenuItem(title: "Quit",
                               action: #selector(NSApplication.terminate(_:)),
                               keyEquivalent: "q"))
    appItem.submenu = appMenu

    let windowItem = NSMenuItem(title: "Window", action: nil, keyEquivalent: "")
    let windowMenu = NSMenu(title: "Window")
    let arrangeItem = NSMenuItem(title: "Arrange", action: nil, keyEquivalent: "")
    let arrangeMenu = NSMenu(title: "Arrange")
    // A real key equivalent, so a chord can be validated against the window
    // the way NSMenu does it: AppKit only enables this item while the harness
    // window is the application's key window.
    let leftItem = NSMenuItem(
        title: "Left",
        action: #selector(HarnessWindowController.onArrangeLeft(_:)),
        keyEquivalent: "l"
    )
    leftItem.keyEquivalentModifierMask = [.command, .option]
    leftItem.target = target
    leftItem.setAccessibilityIdentifier("menu-window-arrange-left")
    arrangeMenu.addItem(leftItem)
    arrangeItem.submenu = arrangeMenu
    windowMenu.addItem(arrangeItem)
    windowItem.submenu = windowMenu
    main.addItem(windowItem)
    NSApp.mainMenu = main
    NSApp.windowsMenu = windowMenu
}

// MARK: - Entry

final class SingleClickReceiver: NSView {
    let journal: URL

    init(frame: NSRect, journal: URL) {
        self.journal = journal
        super.init(frame: frame)
        wantsLayer = true
        layer?.backgroundColor = NSColor.systemGreen.cgColor
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is unsupported") }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    func append(_ value: [String: Any]) {
        var data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        data.append(0x0a)
        let file = try! FileHandle(forWritingTo: journal)
        defer { try! file.close() }
        try! file.seekToEnd()
        try! file.write(contentsOf: data)
    }

    func record(_ kind: String, _ event: NSEvent) {
        guard let window else { fatalError("receiver has no window") }
        append([
            "kind": kind,
            "timestamp": event.timestamp,
            "window_id": event.windowNumber,
            "click_count": event.clickCount,
            "x": event.locationInWindow.x,
            "y": window.frame.height - event.locationInWindow.y
        ])
    }

    override func mouseDown(with event: NSEvent) { record("down", event) }
    override func mouseUp(with event: NSEvent) { record("up", event) }
}

@main
struct CuaAppKitHarness {
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        if let directory = ProcessInfo.processInfo.environment["CUA_APPKIT_SNAPSHOT_DIR"] {
            let fixture = SnapshotPublicationFixture(directory: URL(fileURLWithPath: directory))
            fixture.show()
            app.activate(ignoringOtherApps: true)
            app.run()
            withExtendedLifetime(fixture) {}
            return
        }
        let controller = HarnessWindowController()
        installMenuBar(target: controller)
        controller.show()
        if ProcessInfo.processInfo.environment["CUA_APPKIT_KEEP_ORDERED_FRONT"] == "1" {
            _ = Timer.scheduledTimer(withTimeInterval: 0.01, repeats: true) { [weak window = controller.window] _ in
                window?.orderFrontRegardless()
            }
        }
        if let path = ProcessInfo.processInfo.environment["CUA_APPKIT_POINTER_ORACLE"] {
            let receiver = SingleClickReceiver(
                frame: controller.window.contentView!.bounds,
                journal: URL(fileURLWithPath: path)
            )
            controller.window.contentView = receiver
            receiver.append([
                "kind": "ready",
                "window_id": controller.window.windowNumber,
                "width": controller.window.frame.width,
                "height": controller.window.frame.height
            ])
        }
        var matrixWindows: BringToFrontMatrixWindows?
        if let mode = ProcessInfo.processInfo.environment["CUA_HARNESS_BRING_TO_FRONT_MODE"] {
            matrixWindows = BringToFrontMatrixWindows(parent: controller.window, mode: mode)
        }
        // A second ordinary window that takes key at launch, so the harness
        // window is on screen and frontmost-owned but not key: its
        // `Window > Arrange > Left` key equivalent then validates false
        // exactly the way Notes' Edit > Find items do until the window is key.
        var secondKeyWindow: NSWindow?
        if ProcessInfo.processInfo.environment["CUA_APPKIT_SECOND_KEY_WINDOW"] == "1" {
            let candidate = NSWindow(
                contentRect: NSRect(x: 60, y: 60, width: 420, height: 240),
                styleMask: [.titled, .closable], backing: .buffered, defer: false)
            candidate.title = kSecondKeyWindowTitle
            candidate.isReleasedWhenClosed = false
            candidate.isRestorable = false
            candidate.contentView = NSTextField(labelWithString: "second key window")
            candidate.makeKeyAndOrderFront(nil)
            secondKeyWindow = candidate
        }
        app.activate(ignoringOtherApps: true)
        writeBringToFrontWindowReport(main: controller.window, matrix: matrixWindows)
        app.run()
        _ = matrixWindows
        _ = secondKeyWindow
    }
}
