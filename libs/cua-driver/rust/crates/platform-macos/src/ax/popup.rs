//! The options of an `AXPopUpButton`, and the menu that carries them.
//!
//! A popup's options are the items of its `AXMenu`, not its direct children:
//! once the menu is open the button has exactly one child, a titleless
//! `AXMenu`, and a closed native popup has no children at all — AppKit builds
//! the menu's accessibility items only while it is open. A web `<select>` may
//! instead publish its options as direct children; those count too.

use super::bindings::{copy_children, copy_string_attr, AXUIElementRef};
use super::cache::RetainedElement;
use core_foundation::base::CFRelease;

/// One option a popup offers, retained so it can still be pressed after the
/// walk that found it.
pub struct PopupOption {
    pub element: RetainedElement,
    pub title: String,
    pub value: String,
}

impl PopupOption {
    /// The text the option is chosen by — see [`option_name`].
    pub fn name(&self) -> &str {
        option_name(&self.title, &self.value)
    }

    /// How advice and refusals quote the option — see [`option_label`].
    pub fn label(&self) -> String {
        option_label(&self.title, &self.value)
    }
}

/// The text an option is chosen by: its title, or its value when it has no
/// title (a web `<option>` can publish only a value).
pub fn option_name<'a>(title: &'a str, value: &'a str) -> &'a str {
    if title.is_empty() {
        value
    } else {
        title
    }
}

/// An option quoted by the text it is chosen by, with its value alongside
/// when that differs.
pub fn option_label(title: &str, value: &str) -> String {
    let name = option_name(title, value);
    if value.is_empty() || value == name {
        format!("\"{name}\"")
    } else {
        format!("\"{name}\" (value: {value})")
    }
}

/// Every option `popup` offers right now, in menu order. Separators — items
/// with neither a title nor a value — are not options.
///
/// # Safety
///
/// `popup` must be a live AX element; every child copied here is released or
/// handed to a returned [`RetainedElement`].
pub unsafe fn copy_popup_options(popup: AXUIElementRef) -> Vec<PopupOption> {
    let mut options = Vec::new();
    for child in copy_children(popup) {
        if copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            for item in copy_children(child) {
                push_option(&mut options, item);
            }
            CFRelease(child as _);
        } else {
            push_option(&mut options, child);
        }
    }
    options
}

/// Take ownership of `element` as an option, or release it when it is a
/// separator.
unsafe fn push_option(options: &mut Vec<PopupOption>, element: AXUIElementRef) {
    let title = copy_string_attr(element, "AXTitle").unwrap_or_default();
    let value = copy_string_attr(element, "AXValue").unwrap_or_default();
    if title.is_empty() && value.is_empty() {
        CFRelease(element as _);
        return;
    }
    if let Some(element) = RetainedElement::adopt(element) {
        options.push(PopupOption {
            element,
            title,
            value,
        });
    }
}

/// The popup's open `AXMenu`, if its menu is open.
///
/// # Safety
///
/// `popup` must be a live AX element.
pub unsafe fn copy_open_menu(popup: AXUIElementRef) -> Option<RetainedElement> {
    let mut menu = None;
    for child in copy_children(popup) {
        if menu.is_none() && copy_string_attr(child, "AXRole").as_deref() == Some("AXMenu") {
            menu = RetainedElement::adopt(child);
        } else {
            CFRelease(child as _);
        }
    }
    menu
}

/// Which option a requested value chooses.
#[derive(Debug, PartialEq, Eq)]
pub enum OptionChoice {
    /// Exactly one option is named by exactly this text.
    Unique(usize),
    /// No option is. `near` is the one option that differs from the request
    /// only in case and surrounding whitespace, when there is exactly one.
    Missing { near: Option<usize> },
    /// Several options share the name, so the text cannot say which.
    Ambiguous,
}

/// Choose the option named exactly `requested`. Loose matches are never
/// chosen — a near miss is only offered back — because a popup's options can
/// differ by case alone and the choice is the app's state, not a search.
pub fn choose_option<'a>(names: impl IntoIterator<Item = &'a str>, requested: &str) -> OptionChoice {
    let names: Vec<&str> = names.into_iter().collect();
    let mut exact = names
        .iter()
        .enumerate()
        .filter(|(_, name)| **name == requested);
    match (exact.next(), exact.next()) {
        (Some((index, _)), None) => OptionChoice::Unique(index),
        (Some(_), Some(_)) => OptionChoice::Ambiguous,
        (None, _) => {
            let wanted = requested.trim().to_lowercase();
            let mut near = names
                .iter()
                .enumerate()
                .filter(|(_, name)| name.trim().to_lowercase() == wanted);
            OptionChoice::Missing {
                near: match (near.next(), near.next()) {
                    (Some((index, _)), None) => Some(index),
                    _ => None,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_name_is_chosen() {
        let names = ["None", "Every Day", "Every Week"];
        assert_eq!(choose_option(names, "Every Week"), OptionChoice::Unique(2));
        assert_eq!(
            choose_option(names, "every week"),
            OptionChoice::Missing { near: Some(2) }
        );
        assert_eq!(
            choose_option(names, "Weekly"),
            OptionChoice::Missing { near: None }
        );
    }

    /// Options that differ only by case are distinct choices, so neither is a
    /// near miss for a request matching both loosely.
    #[test]
    fn a_near_miss_is_offered_only_when_it_is_the_only_one() {
        assert_eq!(
            choose_option(["JPEG", "jpeg"], "Jpeg"),
            OptionChoice::Missing { near: None }
        );
    }

    #[test]
    fn a_shared_name_is_ambiguous() {
        assert_eq!(
            choose_option(["Custom…", "None", "Custom…"], "Custom…"),
            OptionChoice::Ambiguous
        );
    }

    /// A web `<option>` with no title is chosen, and quoted, by its value.
    #[test]
    fn an_untitled_option_goes_by_its_value() {
        assert_eq!(option_name("", "fr"), "fr");
        assert_eq!(option_label("", "fr"), "\"fr\"");
        assert_eq!(option_label("French", "fr"), "\"French\" (value: fr)");
    }
}
