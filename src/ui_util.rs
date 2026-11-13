//! Small GTK helpers shared by dialogs, toasts and status surfaces.

use libadwaita as adw;

/// Construct a toast that treats its title as plain text. Dynamic content is
/// data, never Pango markup.
pub(crate) fn plain_toast(message: &str) -> adw::Toast {
    adw::Toast::builder()
        .title(message)
        .use_markup(false)
        .build()
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
}
