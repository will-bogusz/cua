//! Background keyboard synthesis via SLEventPostToPid (SkyLight SPI),
//! with fallback to the public CGEvent::post_to_pid.
//!
//! For keyboard events, `post_to_pid` attaches an `SLSEventAuthenticationMessage`
//! so Chromium-based apps accept synthetic keystrokes as trusted live input
//! (required on macOS 14+ for VS Code, Chrome, Electron apps).

use core_graphics::{
    event::{CGEvent, CGEventFlags},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use cua_driver_core::operation;
use foreign_types::ForeignType;

const SCREEN_SHARING_BUNDLE_ID: &str = "com.apple.ScreenSharing";
const SHIFT_KEY_CODE: u16 = 56;

fn is_screen_sharing_bundle_id(bundle_id: &str) -> bool {
    bundle_id == SCREEN_SHARING_BUNDLE_ID
}

/// Whether `pid` is Apple's Screen Sharing client.
///
/// Screen Sharing is an input forwarder rather than a text consumer: it relays
/// physical virtual-key transitions to the guest and ignores the Unicode string
/// attached to a synthetic keycode-0 event. Keep that special case explicit so
/// ordinary PID-routed text input retains its layout-independent Unicode path.
pub fn is_screen_sharing_pid(pid: i32) -> bool {
    crate::apps::bundle_id_for_pid(pid)
        .as_deref()
        .is_some_and(is_screen_sharing_bundle_id)
}

/// Press and release a single key, delivered to `pid` without stealing focus.
pub fn press_key(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    operation::check()?;
    let (key, flags) = if key == "+" || key.eq_ignore_ascii_case("plus") {
        ("=", modifier_flags(modifiers) | modifier_flags(&["shift"]))
    } else {
        (key, modifier_flags(modifiers))
    };
    prepared_key_pair(
        KeyDelivery::Pid {
            pid,
            authenticated: true,
        },
        key_name_to_code(key)?,
        flags,
    )
}

/// Type a string character-by-character to `pid`.
pub fn type_text(pid: i32, text: &str) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;

    for ch in text.chars() {
        operation::check()?;
        let ch_str = ch.to_string();
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard down failed"))?;
        down.set_string(&ch_str);
        // Always zero flags: Chrome inspects the flags field to infer modifier
        // state; without this, uppercase chars (e.g. 'E') are seen as Shift+e
        // and the modifier leaks into the next character (Swift fix: event.flags = []).
        down.set_flags(CGEventFlags::CGEventFlagNull);
        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard up failed"))?;
        up.set_string(&ch_str);
        up.set_flags(CGEventFlags::CGEventFlagNull);
        post_keyboard_event(pid, &down);
        std::thread::sleep(std::time::Duration::from_millis(8));

        post_keyboard_event(pid, &up);
        operation::sleep(std::time::Duration::from_millis(8))?;
    }
    Ok(())
}

/// Type a string character-by-character with an extra `inter_char_delay_ms`
/// pause after each character (on top of the internal 8 ms down/up gap).
pub fn type_text_with_delay(pid: i32, text: &str, inter_char_delay_ms: u64) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;

    for ch in text.chars() {
        operation::check()?;
        let ch_str = ch.to_string();
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard down failed"))?;
        down.set_string(&ch_str);
        down.set_flags(CGEventFlags::CGEventFlagNull);
        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard up failed"))?;
        up.set_string(&ch_str);
        up.set_flags(CGEventFlags::CGEventFlagNull);
        post_keyboard_event(pid, &down);
        std::thread::sleep(std::time::Duration::from_millis(8));

        post_keyboard_event(pid, &up);

        // Additional inter-character delay on top of the 8 ms internal gap.
        if inter_char_delay_ms > 0 {
            operation::sleep(std::time::Duration::from_millis(inter_char_delay_ms))?;
        } else {
            operation::sleep(std::time::Duration::from_millis(8))?;
        }
    }
    Ok(())
}

/// Send a key combination (hotkey) to `pid`.
pub fn hotkey(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    press_key(pid, key, modifiers)
}

/// Send a key combination to `pid` WITHOUT the auth-message envelope.
///
/// Required for NSMenu key equivalents: with the envelope, SLEventPostToPid
/// forks onto a direct-mach path that bypasses IOHIDPostEvent — NSMenu never
/// sees those events. Without the envelope the path goes through IOHIDPostEvent
/// so NSApplication.sendEvent: dispatches NSMenu key equivalents.
pub fn hotkey_no_auth(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    press_key_no_auth(pid, key, modifiers)
}

pub fn press_key_no_auth(pid: i32, key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    prepared_key_pair(
        KeyDelivery::Pid {
            pid,
            authenticated: false,
        },
        key_name_to_code(key)?,
        modifier_flags(modifiers),
    )
}

/// Foreground chords hold only their prepared modifier pairs, then release
/// them in reverse order even if base-key preparation or the gesture fails.
pub fn press_key_global(key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    let code = key_name_to_code(key)?;
    with_global_modifier_keys(modifiers, |flags| {
        prepared_key_pair(KeyDelivery::Global, code, flags)
    })
}

#[derive(Clone, Copy)]
enum KeyDelivery {
    Global,
    Pid { pid: i32, authenticated: bool },
}
impl KeyDelivery {
    fn post(self, event: &CGEvent) {
        match self {
            Self::Global => event.post(core_graphics::event::CGEventTapLocation::HID),
            Self::Pid {
                pid,
                authenticated: true,
            } => post_keyboard_event(pid, event),
            Self::Pid {
                pid,
                authenticated: false,
            } => {
                if !crate::input::skylight::post_to_pid(
                    pid as libc::pid_t,
                    event.as_ptr() as *mut std::ffi::c_void,
                    false,
                ) {
                    event.post_to_pid(pid as libc::pid_t);
                }
            }
        }
    }
}

fn prepare_key_event(
    source: &CGEventSource,
    code: u16,
    down: bool,
    flags: CGEventFlags,
) -> anyhow::Result<CGEvent> {
    let event = CGEvent::new_keyboard_event(source.clone(), code, down)
        .map_err(|_| anyhow::anyhow!("keyboard event preparation failed"))?;
    event.set_flags(flags);
    Ok(event)
}

/// Allocation completes before the matching down can be sent. The same
/// prepared release is used on success, cancellation and unwind.
fn prepared_key_pair(delivery: KeyDelivery, code: u16, flags: CGEventFlags) -> anyhow::Result<()> {
    operation::check()?;
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let pair = (
        prepare_key_event(&source, code, true, flags)?,
        prepare_key_event(&source, code, false, flags)?,
    );
    with_pressed_pairs(&[pair], |event| delivery.post(event), || Ok(()))
}

/// Native posting is infallible after event preparation. This small generic
/// state machine is also exercised against a receiver journal without sending
/// test keys into the user's desktop.
fn with_pressed_pairs<E, R>(
    pairs: &[(E, E)],
    post: impl Fn(&E),
    gesture: impl FnOnce() -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    let pressed = std::cell::Cell::new(0);
    let release = operation::ReleaseOnDrop::new(|| {
        for (_, up) in pairs.iter().take(pressed.get()).rev() {
            post(up);
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
    });
    for (down, _) in pairs {
        operation::check()?;
        pressed.set(pressed.get() + 1);
        post(down);
        operation::sleep(std::time::Duration::from_millis(8))?;
    }
    operation::check()?;
    let result = gesture();
    drop(release);
    result
}

fn with_modifier_keys<R>(
    delivery: KeyDelivery,
    modifiers: &[&str],
    gesture: impl FnOnce(CGEventFlags) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    operation::check()?;
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    let mut flags = CGEventFlags::CGEventFlagNull;
    let mut codes = Vec::new();
    let mut pairs = Vec::new();
    for modifier in modifiers {
        let Some((code, flag)) = modifier_key_code_and_flag(modifier) else {
            continue;
        };
        if codes.contains(&code) {
            continue;
        }
        codes.push(code);
        let previous = flags;
        flags |= flag;
        pairs.push((
            prepare_key_event(&source, code, true, flags)?,
            prepare_key_event(&source, code, false, previous)?,
        ));
    }
    with_pressed_pairs(&pairs, |event| delivery.post(event), || gesture(flags))
}

pub(super) fn with_global_modifier_keys<R>(
    modifiers: &[&str],
    gesture: impl FnOnce(CGEventFlags) -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    with_modifier_keys(KeyDelivery::Global, modifiers, gesture)
}
pub(super) fn with_pid_modifier_keys<R>(
    pid: i32,
    modifiers: &[&str],
    gesture: impl FnOnce() -> anyhow::Result<R>,
) -> anyhow::Result<R> {
    with_modifier_keys(
        KeyDelivery::Pid {
            pid,
            authenticated: true,
        },
        modifiers,
        |_| gesture(),
    )
}

fn modifier_key_code_and_flag(modifier: &str) -> Option<(u16, CGEventFlags)> {
    match modifier.to_lowercase().as_str() {
        "cmd" | "command" => Some((55, CGEventFlags::CGEventFlagCommand)),
        "shift" => Some((56, CGEventFlags::CGEventFlagShift)),
        "option" | "alt" => Some((58, CGEventFlags::CGEventFlagAlternate)),
        "ctrl" | "control" => Some((59, CGEventFlags::CGEventFlagControl)),
        "fn" => Some((63, CGEventFlags::CGEventFlagSecondaryFn)),
        _ => None,
    }
}

/// Type Unicode text into the frontmost application through the global HID
/// queue. This is the desktop-scope counterpart to PID-routed `type_text` and
/// mirrors computer-server's frontmost pynput typing behavior.
pub fn type_text_global(text: &str, inter_char_delay_ms: u64) -> anyhow::Result<()> {
    use core_graphics::event::CGEventTapLocation;

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("CGEventSource::new failed"))?;
    for ch in text.chars() {
        operation::check()?;
        let value = ch.to_string();
        let down = CGEvent::new_keyboard_event(source.clone(), 0, true)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard down failed"))?;
        down.set_string(&value);
        down.set_flags(CGEventFlags::CGEventFlagNull);
        let up = CGEvent::new_keyboard_event(source.clone(), 0, false)
            .map_err(|_| anyhow::anyhow!("CGEvent keyboard up failed"))?;
        up.set_string(&value);
        up.set_flags(CGEventFlags::CGEventFlagNull);
        down.post(CGEventTapLocation::HID);
        std::thread::sleep(std::time::Duration::from_millis(8));
        up.post(CGEventTapLocation::HID);
        operation::sleep(std::time::Duration::from_millis(inter_char_delay_ms.max(8)))?;
    }
    Ok(())
}

/// Type ASCII text through the global HID queue as physical US-keyboard
/// transitions.
///
/// This is reserved for a caller that has guarded the exact target with
/// `with_foreground_hid_activation`. Remote-input clients such as Screen
/// Sharing forward virtual keycodes and modifier transitions, not the Unicode
/// payload carried by keycode 0, so the ordinary text synthesis path cannot be
/// used for them.
pub fn type_text_physical_global(text: &str, inter_char_delay_ms: u64) -> anyhow::Result<()> {
    operation::check()?;
    use core_graphics::event::CGEventTapLocation;

    // Validate the complete payload before posting its first event. A string
    // containing an unsupported character must fail without partially typing.
    let event_groups = text
        .chars()
        .map(physical_text_events)
        .collect::<anyhow::Result<Vec<_>>>()?;
    for events in event_groups {
        operation::check()?;
        let native_events = events
            .iter()
            .map(|event| create_bare_keyboard_event(event.key_code, event.key_down))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for cg_event in native_events {
            cg_event.post(CGEventTapLocation::HID);
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        if inter_char_delay_ms > 8 {
            std::thread::sleep(std::time::Duration::from_millis(inter_char_delay_ms - 8));
        }
    }
    Ok(())
}

/// Send a physical key chord as the virtual-key transition sequence Apple
/// documents for `CGEventCreateKeyboardEvent`: NULL source, modifier downs,
/// base down/up, then modifier ups in reverse order.
///
/// Each transition also carries the chord's accumulated `CGEventFlags`, as the
/// PID-routed and global rungs do. A keyboard event created from the default
/// source starts with no flags, and posting a modifier keycode does not
/// retro-fit them onto events that were already created, so a chord whose base
/// key carries no flags arrives as the bare key: `cmd+a` inserts a literal `a`.
/// Remote-input clients such as Screen Sharing re-derive modifiers from the
/// keycode transitions and ignore the flags, so both consumers are served.
pub fn press_key_bare_global(key: &str, modifiers: &[&str]) -> anyhow::Result<()> {
    operation::check()?;
    use core_graphics::event::CGEventTapLocation;

    let key_code = key_name_to_code(key)?;
    let mut modifier_keys = Vec::new();
    for modifier in modifiers {
        let Some((modifier_code, flag)) = modifier_key_code_and_flag(modifier) else {
            continue;
        };
        if !modifier_keys.iter().any(|&(code, _)| code == modifier_code) {
            modifier_keys.push((modifier_code, flag));
        }
    }

    let events = bare_chord_transitions(key_code, &modifier_keys)
        .into_iter()
        .map(|(code, down, flags)| {
            let event = create_bare_keyboard_event(code, down)?;
            event.set_flags(flags);
            Ok(event)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for event in events {
        event.post(CGEventTapLocation::HID);
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    Ok(())
}

/// The chord's transitions with the flags each one carries. A modifier's own
/// down is the first event that holds its flag and its up is the first that has
/// dropped it again, so the base key always sees the full chord.
fn bare_chord_transitions(
    key_code: u16,
    modifier_keys: &[(u16, CGEventFlags)],
) -> Vec<(u16, bool, CGEventFlags)> {
    let mut transitions = Vec::with_capacity(modifier_keys.len() * 2 + 2);
    let mut flags = CGEventFlags::CGEventFlagNull;
    let mut releases = Vec::with_capacity(modifier_keys.len());
    for &(code, flag) in modifier_keys {
        let previous = flags;
        flags |= flag;
        transitions.push((code, true, flags));
        releases.push((code, false, previous));
    }
    transitions.push((key_code, true, flags));
    transitions.push((key_code, false, flags));
    transitions.extend(releases.into_iter().rev());
    transitions
}

fn create_bare_keyboard_event(key_code: u16, key_down: bool) -> anyhow::Result<CGEvent> {
    unsafe {
        let event_ref = CGEventCreateKeyboardEvent(std::ptr::null_mut(), key_code, key_down);
        if event_ref.is_null() {
            anyhow::bail!("CGEventCreateKeyboardEvent with default source failed");
        }
        Ok(CGEvent::from_ptr(event_ref))
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateKeyboardEvent(
        source: core_graphics::sys::CGEventSourceRef,
        key_code: u16,
        key_down: bool,
    ) -> core_graphics::sys::CGEventRef;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PhysicalTextEvent {
    key_code: u16,
    key_down: bool,
    event_type: PhysicalEventType,
    shift: bool,
    text: Option<char>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhysicalEventType {
    KeyDown,
    KeyUp,
    FlagsChanged,
}

fn physical_text_events(ch: char) -> anyhow::Result<Vec<PhysicalTextEvent>> {
    let (key_code, shift) = physical_key_for_char(ch).ok_or_else(|| {
        anyhow::anyhow!(
            "Screen Sharing physical text delivery does not support character U+{:04X}",
            ch as u32
        )
    })?;
    let mut events = Vec::with_capacity(if shift { 4 } else { 2 });
    if shift {
        events.push(PhysicalTextEvent {
            key_code: SHIFT_KEY_CODE,
            key_down: true,
            event_type: PhysicalEventType::FlagsChanged,
            shift: true,
            text: None,
        });
    }
    events.push(PhysicalTextEvent {
        key_code,
        key_down: true,
        event_type: PhysicalEventType::KeyDown,
        shift,
        text: Some(ch),
    });
    events.push(PhysicalTextEvent {
        key_code,
        key_down: false,
        event_type: PhysicalEventType::KeyUp,
        shift,
        text: Some(ch),
    });
    if shift {
        events.push(PhysicalTextEvent {
            key_code: SHIFT_KEY_CODE,
            key_down: false,
            event_type: PhysicalEventType::FlagsChanged,
            shift: false,
            text: None,
        });
    }
    Ok(events)
}

/// Map printable ASCII to the physical key that produces it on the standard US
/// layout. The Screen Sharing path intentionally sends only the returned
/// keycode and the required bare Shift transitions.
fn physical_key_for_char(ch: char) -> Option<(u16, bool)> {
    let lower = ch.to_ascii_lowercase();
    if ch.is_ascii_alphabetic() {
        return key_name_to_code(&lower.to_string())
            .ok()
            .map(|code| (code, ch.is_ascii_uppercase()));
    }
    if ch.is_ascii_digit() {
        return key_name_to_code(&ch.to_string())
            .ok()
            .map(|code| (code, false));
    }

    let (base, shift) = match ch {
        ' ' => ("space", false),
        '\t' => ("tab", false),
        '\n' | '\r' => ("return", false),
        '-' => ("-", false),
        '_' => ("-", true),
        '=' => ("=", false),
        '+' => ("=", true),
        '[' => ("[", false),
        '{' => ("[", true),
        ']' => ("]", false),
        '}' => ("]", true),
        '\\' => ("\\", false),
        '|' => ("\\", true),
        ';' => (";", false),
        ':' => (";", true),
        '\'' => ("'", false),
        '"' => ("'", true),
        ',' => (",", false),
        '<' => (",", true),
        '.' => (".", false),
        '>' => (".", true),
        '/' => ("/", false),
        '?' => ("/", true),
        '`' => ("`", false),
        '~' => ("`", true),
        '!' => ("1", true),
        '@' => ("2", true),
        '#' => ("3", true),
        '$' => ("4", true),
        '%' => ("5", true),
        '^' => ("6", true),
        '&' => ("7", true),
        '*' => ("8", true),
        '(' => ("9", true),
        ')' => ("0", true),
        _ => return None,
    };
    key_name_to_code(base).ok().map(|code| (code, shift))
}

/// Post a keyboard event to `pid` via SLEventPostToPid (with auth message for
/// Chromium/Electron support) or fall back to CGEvent::post_to_pid.
pub(super) fn post_keyboard_event(pid: i32, event: &CGEvent) {
    let event_ptr = event.as_ptr() as *mut std::ffi::c_void;
    // attachAuthMessage = true: required for Chromium keyboard on macOS 14+.
    if !crate::input::skylight::post_to_pid(pid as libc::pid_t, event_ptr, true) {
        event.post_to_pid(pid as libc::pid_t);
    }
}

fn modifier_flags(modifiers: &[&str]) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    for m in modifiers {
        match m.to_lowercase().as_str() {
            "cmd" | "command" => flags |= CGEventFlags::CGEventFlagCommand,
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "ctrl" | "control" => flags |= CGEventFlags::CGEventFlagControl,
            "fn" => flags |= CGEventFlags::CGEventFlagSecondaryFn,
            _ => {}
        }
    }
    flags
}

pub(super) fn key_name_to_code(key: &str) -> anyhow::Result<u16> {
    let code = match key.to_lowercase().as_str() {
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "delete" | "backspace" => 51,
        "escape" | "esc" => 53,
        "command" | "cmd" => 55,
        "shift" => 56,
        "capslock" => 57,
        "option" | "alt" => 58,
        "control" | "ctrl" => 59,
        "fn" => 63,
        "home" => 115,
        "pageup" => 116,
        "del" | "forward_delete" => 117,
        "end" => 119,
        "pagedown" => 121,
        "left" | "left_arrow" => 123,
        "right" | "right_arrow" => 124,
        "down" | "down_arrow" => 125,
        "up" | "up_arrow" => 126,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "`" => 50,
        _ => anyhow::bail!("Unknown key name: {key}"),
    };
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::event::CGEventType;

    #[test]
    fn physical_text_uses_flags_changed_for_balanced_shift_transitions() {
        assert_eq!(
            physical_text_events('a').unwrap(),
            vec![
                PhysicalTextEvent {
                    key_code: 0,
                    key_down: true,
                    event_type: PhysicalEventType::KeyDown,
                    shift: false,
                    text: Some('a'),
                },
                PhysicalTextEvent {
                    key_code: 0,
                    key_down: false,
                    event_type: PhysicalEventType::KeyUp,
                    shift: false,
                    text: Some('a'),
                },
            ]
        );
        assert_eq!(
            physical_text_events('Z').unwrap(),
            vec![
                PhysicalTextEvent {
                    key_code: 56,
                    key_down: true,
                    event_type: PhysicalEventType::FlagsChanged,
                    shift: true,
                    text: None,
                },
                PhysicalTextEvent {
                    key_code: 6,
                    key_down: true,
                    event_type: PhysicalEventType::KeyDown,
                    shift: true,
                    text: Some('Z'),
                },
                PhysicalTextEvent {
                    key_code: 6,
                    key_down: false,
                    event_type: PhysicalEventType::KeyUp,
                    shift: true,
                    text: Some('Z'),
                },
                PhysicalTextEvent {
                    key_code: 56,
                    key_down: false,
                    event_type: PhysicalEventType::FlagsChanged,
                    shift: false,
                    text: None,
                },
            ]
        );
        assert_eq!(physical_key_for_char('1'), Some((18, false)));
        assert_eq!(physical_key_for_char('!'), Some((18, true)));
        assert_eq!(physical_key_for_char('/'), Some((44, false)));
        assert_eq!(physical_key_for_char('?'), Some((44, true)));
    }

    #[test]
    fn bare_modifier_events_derive_type_and_flags_from_default_source() {
        let shift_down = create_bare_keyboard_event(SHIFT_KEY_CODE, true).unwrap();
        assert_eq!(
            shift_down.get_type() as u32,
            CGEventType::FlagsChanged as u32
        );
        assert!(
            shift_down
                .get_flags()
                .contains(CGEventFlags::CGEventFlagShift),
            "bare Shift down must derive the active Shift flag"
        );
        assert_eq!(
            shift_down
                .get_integer_value_field(core_graphics::event::EventField::EVENT_SOURCE_STATE_ID),
            CGEventSourceStateID::CombinedSessionState as i64,
            "NULL source must resolve to the default combined-session state, not HID state"
        );

        let shift_up = create_bare_keyboard_event(SHIFT_KEY_CODE, false).unwrap();
        assert_eq!(shift_up.get_type() as u32, CGEventType::FlagsChanged as u32);
        assert!(
            !shift_up
                .get_flags()
                .contains(CGEventFlags::CGEventFlagShift),
            "bare Shift up must derive cleared Shift state"
        );

        let z_down = create_bare_keyboard_event(6, true).unwrap();
        assert_eq!(z_down.get_type() as u32, CGEventType::KeyDown as u32);
        assert_eq!(
            z_down
                .get_integer_value_field(core_graphics::event::EventField::KEYBOARD_EVENT_KEYCODE),
            6
        );
    }

    #[test]
    fn bare_command_chord_orders_modifier_base_and_reverse_release() {
        let command = (55, CGEventFlags::CGEventFlagCommand);
        assert_eq!(
            bare_chord_transitions(9, &[command]),
            vec![
                (55, true, CGEventFlags::CGEventFlagCommand),
                (9, true, CGEventFlags::CGEventFlagCommand),
                (9, false, CGEventFlags::CGEventFlagCommand),
                (55, false, CGEventFlags::CGEventFlagNull),
            ]
        );
    }

    /// The default source derives a modifier's own flag from its keycode but
    /// has nothing to derive from for the base key, which is what turned
    /// `cmd+a` into a literal `a` on the foreground rung.
    #[test]
    fn bare_base_key_carries_the_chord_flags_the_default_source_cannot_derive() {
        let command_down = create_bare_keyboard_event(55, true).unwrap();
        assert_eq!(
            command_down.get_type() as u32,
            CGEventType::FlagsChanged as u32
        );
        assert!(
            command_down
                .get_flags()
                .contains(CGEventFlags::CGEventFlagCommand),
            "bare Command down must derive the active Command flag"
        );

        let bare_base = create_bare_keyboard_event(0, true).unwrap();
        assert!(
            !bare_base
                .get_flags()
                .contains(CGEventFlags::CGEventFlagCommand),
            "the default source cannot derive Command for a non-modifier keycode"
        );

        let (code, down, flags) =
            bare_chord_transitions(0, &[(55, CGEventFlags::CGEventFlagCommand)])[1];
        let carried = create_bare_keyboard_event(code, down).unwrap();
        carried.set_flags(flags);
        assert!(
            carried
                .get_flags()
                .contains(CGEventFlags::CGEventFlagCommand),
            "the chord's base key must carry Command"
        );
    }

    #[test]
    fn physical_text_rejects_characters_without_a_lossless_keycode() {
        let error = physical_text_events('🙂').unwrap_err().to_string();
        assert!(error.contains("U+1F642"), "{error}");
    }

    #[test]
    fn physical_text_covers_printable_ascii_and_common_text_whitespace() {
        for byte in 0x20_u8..=0x7e {
            let ch = char::from(byte);
            assert!(
                physical_key_for_char(ch).is_some(),
                "missing physical key for ASCII {ch:?}"
            );
        }
        for ch in ['\t', '\n', '\r'] {
            assert!(
                physical_key_for_char(ch).is_some(),
                "missing physical key for whitespace {ch:?}"
            );
        }
    }

    #[test]
    fn screen_sharing_detection_is_exact_and_case_sensitive() {
        assert!(is_screen_sharing_bundle_id("com.apple.ScreenSharing"));
        assert!(!is_screen_sharing_bundle_id("com.apple.screensharing"));
        assert!(!is_screen_sharing_bundle_id("com.microsoft.rdc.macos"));
    }
}

#[cfg(test)]
mod held_key_tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_without_modifiers_still_prevents_the_gesture() {
        let control = std::sync::Arc::new(operation::Cancellation::default());
        control.cancel();
        let called = std::cell::Cell::new(false);
        let result = operation::scope(control, async {
            with_pressed_pairs::<u8, _>(
                &[],
                |_| panic!("no keys"),
                || {
                    called.set(true);
                    Ok(())
                },
            )
        })
        .await;
        assert!(result.is_err());
        assert!(!called.get());
    }

    use std::cell::RefCell;
    use std::sync::Arc;

    #[tokio::test]
    async fn cancellation_releases_only_pressed_keys_without_entering_the_gesture() {
        let control = Arc::new(operation::Cancellation::default());
        let cancel = control.clone();
        let journal = RefCell::new(Vec::new());
        let result = operation::scope(control, async {
            with_pressed_pairs(
                &[("shift-down", "shift-up"), ("cmd-down", "cmd-up")],
                |event| {
                    journal.borrow_mut().push(*event);
                    if *event == "shift-down" {
                        cancel.cancel();
                    }
                },
                || -> anyhow::Result<()> { panic!("cancelled gesture must not run") },
            )
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*journal.borrow(), ["shift-down", "shift-up"]);
    }
    #[test]
    fn callback_error_releases_modifiers_in_reverse_order() {
        let journal = RefCell::new(Vec::new());
        let result = with_pressed_pairs(
            &[("shift-down", "shift-up"), ("cmd-down", "cmd-up")],
            |event| journal.borrow_mut().push(*event),
            || -> anyhow::Result<()> { anyhow::bail!("base key preparation failed") },
        );
        assert!(result.is_err());
        assert_eq!(
            *journal.borrow(),
            ["shift-down", "cmd-down", "cmd-up", "shift-up"]
        );
    }
    #[test]
    fn callback_unwind_releases_all_owned_keys() {
        let journal = RefCell::new(Vec::new());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_pressed_pairs(
                &[("down", "up")],
                |event| journal.borrow_mut().push(*event),
                || -> anyhow::Result<()> { panic!("gesture unwound") },
            )
        }));
        assert!(result.is_err());
        assert_eq!(*journal.borrow(), ["down", "up"]);
    }
}
