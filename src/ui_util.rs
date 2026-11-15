//! Small GTK helpers shared by dialogs, toasts and status surfaces.

use gettextrs::gettext;
use gtk4::prelude::*;
use libadwaita as adw;

/// Construct a toast that treats its title as plain text. Dynamic content is
/// data, never Pango markup.
pub(crate) fn plain_toast(message: &str) -> adw::Toast {
    adw::Toast::builder()
        .title(message)
        .use_markup(false)
        .build()
}

/// Undo toast whose button sits next to its message.
///
/// `AdwToast` pins a toast with a built-in action button to 450px, which
/// strands the button away from short messages (beta.20 report). A custom
/// title keeps the pill as wide as its content; the theme styles the plain
/// button like the built-in one.
pub(crate) fn undo_toast(message: &str, on_undo: impl Fn() + 'static) -> adw::Toast {
    let toast = plain_toast(message);

    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let label = gtk4::Label::new(Some(message));
    label.add_css_class("heading");
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_single_line_mode(true);
    let button = gtk4::Button::with_label(&gettext("Undo"));
    button.set_valign(gtk4::Align::Center);
    row.append(&label);
    row.append(&button);
    toast.set_custom_title(Some(&row));

    let toast_for_click = toast.clone();
    button.connect_clicked(move |button| {
        button.set_sensitive(false);
        toast_for_click.dismiss();
        on_undo();
    });

    toast
}

/// Substitute named placeholders in one pass. Inserted values are appended
/// verbatim and are never scanned again for placeholder-like text.
pub(crate) fn format_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some(start) = remaining.find('{') {
        output.push_str(&remaining[..start]);
        let tail = &remaining[start..];
        let replacement = values
            .iter()
            .find(|(key, _)| tail.starts_with(&format!("{{{key}}}")));
        if let Some((key, value)) = replacement {
            output.push_str(value);
            remaining = &tail[key.len() + 2..];
        } else {
            output.push('{');
            remaining = &tail[1..];
        }
    }
    output.push_str(remaining);
    output
}

#[cfg(test)]
mod tests {
    use super::format_template;

    #[test]
    fn placeholder_like_inserted_values_are_not_rescanned() {
        assert_eq!(
            format_template(
                "Could not {verb} {what}: {e}",
                &[
                    ("verb", "open"),
                    ("what", "{e}.mkv"),
                    ("e", "permission denied"),
                ],
            ),
            "Could not open {e}.mkv: permission denied"
        );
    }

    #[test]
    fn unknown_placeholders_are_preserved() {
        assert_eq!(
            format_template("Value {known} {unknown}", &[("known", "ok")]),
            "Value ok {unknown}"
        );
    }

    // A display is needed to build the toast widgets; the test is `#[ignore]`d
    // for the plain run and CI runs it under Xvfb (see ci.yml).
    #[test]
    #[ignore = "needs a display; run under Xvfb (see ci.yml)"]
    fn undo_toast_keeps_its_button_next_to_the_message() {
        gtk4::init().expect("gtk::init (run under Xvfb)");
        let toast = super::undo_toast("Removed recipe.mp4", || {});

        // The built-in action button pins the toast to 450px (libadwaita's
        // NATURAL_WIDTH), which strands the button away from the message.
        assert!(toast.button_label().is_none());
        assert!(toast.custom_title().is_some());
    }
}
