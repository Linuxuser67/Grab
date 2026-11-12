//! Small GTK dialog helpers; leaf (gtk/adw only) breaking the `window ↔ install_help` cycle.

use gtk4::glib;

/// Escape user-controlled text before it is interpolated into an `AdwToast`.
/// Toasts parse Pango markup by default; AlertDialog headings and bodies are
/// plain text unless markup is explicitly enabled.
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
