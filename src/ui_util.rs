//! Small GTK dialog helpers; leaf (gtk/adw only) breaking the `window ↔ install_help` cycle.

use gettextrs::gettext;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

/// Shared copyable-details popup used by failure dialogs across the app. This
/// deliberately uses the established AdwAlertDialog + read-only TextView
/// pattern so each feature can supply its own heading, explanation and report.
pub(crate) fn copyable_details_dialog<P: IsA<gtk4::Widget>>(
    parent: &P,
    heading: &str,
    body: &str,
    display: &str,
    min_content_height: i32,
) -> adw::AlertDialog {
    let heading = esc_markup(heading);
    let body = esc_markup(body);
    let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));

    let text = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .build();
    text.buffer().set_text(display);
    let scrolled = gtk4::ScrolledWindow::builder()
        .min_content_height(min_content_height)
        .child(&text)
        .build();
    dialog.set_extra_child(Some(&scrolled));

    dialog.add_responses(&[("copy", &gettext("Copy")), ("close", &gettext("Close"))]);
    dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");

    let parent: &gtk4::Widget = parent.upcast_ref();
    let clipboard = parent.clipboard();
    let display = display.to_owned();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "copy" {
            clipboard.set_text(&display);
        }
    });

    dialog.present(Some(parent));
    dialog
}

/// Escape user-controlled text before it is interpolated into a markup-parsing
/// widget (`AdwToast` titles and `AdwAlertDialog` heading/body both take Pango
/// markup): filenames, paths, URLs and backend error strings routinely contain
/// `&`, `<` or `>`, which would otherwise abort the whole label with a
/// Gtk-WARNING and leave it blank.
pub(crate) fn esc_markup(s: &str) -> String {
    glib::markup_escape_text(s).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_markup_neutralizes_pango_metachars() {
        assert_eq!(esc_markup("a & b"), "a &amp; b");
        assert_eq!(esc_markup("<tag>"), "&lt;tag&gt;");
        assert_eq!(esc_markup("plain name.mp4"), "plain name.mp4");
    }
}
