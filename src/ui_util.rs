//! Small GTK dialog helpers; leaf (gtk/adw only) breaking the `window ↔ install_help` cycle.

use adw::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

/// Escape user-controlled text before it is interpolated into a markup-parsing
/// widget (`AdwToast` titles and `AdwAlertDialog` heading/body both take Pango
/// markup): filenames, paths, URLs and backend error strings routinely contain
/// `&`, `<` or `>`, which would otherwise abort the whole label with a
/// Gtk-WARNING and leave it blank.
pub(crate) fn esc_markup(s: &str) -> String {
    glib::markup_escape_text(s).into()
}

/// Close the dialog when the button is clicked (Cancel/close actions).
pub(crate) fn close_on_click(btn: &gtk4::Button, dialog: &adw::Dialog) {
    let weak = dialog.downgrade();
    btn.connect_clicked(move |_| {
        if let Some(d) = weak.upgrade() {
            d.close();
        }
    });
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
