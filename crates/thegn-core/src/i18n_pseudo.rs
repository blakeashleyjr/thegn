//! Developer pseudolocale derived from the canonical Fluent source.

use std::borrow::Cow;
use std::collections::HashMap;

use fluent_templates::FluentBundle;
use fluent_templates::fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use once_cell::sync::Lazy;

use crate::i18n_parity::DEFAULT_SOURCE;

static BUNDLE: Lazy<Option<FluentBundle<FluentResource>>> =
    Lazy::new(|| build_bundle(DEFAULT_SOURCE));

fn build_bundle(source: &str) -> Option<FluentBundle<FluentResource>> {
    let resource = FluentResource::try_new(source.to_string()).ok()?;
    let locale = "en-XA".parse().ok()?;
    let mut bundle = FluentBundle::new_concurrent(vec![locale]);
    // Fluent invokes this only for text elements, never identifiers, selector
    // syntax, placeables, or the argument values supplied by the caller.
    bundle.set_transform(Some(pseudolocalize_text));
    bundle.add_resource(resource).ok()?;
    Some(bundle)
}

fn pseudolocalize_text(value: &str) -> Cow<'_, str> {
    Cow::Owned(pseudolocalize_value(value))
}

pub(crate) fn lookup(
    key: &str,
    args: Option<&HashMap<Cow<'static, str>, FluentValue<'_>>>,
) -> Option<String> {
    let bundle = BUNDLE.as_ref()?;
    let message = bundle.get_message(key)?;
    let value = message.value()?;
    let fluent_args = args.map(|args| {
        args.iter()
            .map(|(key, value)| (key.as_ref(), value.clone()))
            .collect::<FluentArgs<'_>>()
    });
    let mut errors = Vec::new();
    let rendered = bundle.format_pattern(value, fluent_args.as_ref(), &mut errors);
    errors.is_empty().then(|| rendered.into_owned())
}

fn pseudolocalize_value(value: &str) -> String {
    let mut output = String::with_capacity(value.len() * 2);
    let mut placeable_depth = 0_u32;
    for ch in value.chars() {
        match ch {
            '{' => {
                placeable_depth += 1;
                output.push(ch);
            }
            '}' if placeable_depth > 0 => {
                placeable_depth -= 1;
                output.push(ch);
            }
            _ if placeable_depth > 0 => output.push(ch),
            _ => push_expanded(&mut output, ch),
        }
    }
    output
}

fn push_expanded(output: &mut String, ch: char) {
    let accented = match ch {
        'A' => 'Å',
        'a' => 'à',
        'E' => 'É',
        'e' => 'é',
        'I' => 'Î',
        'i' => 'ï',
        'O' => 'Ö',
        'o' => 'ö',
        'U' => 'Û',
        'u' => 'ü',
        'C' => 'Ç',
        'c' => 'ç',
        'N' => 'Ñ',
        'n' => 'ñ',
        'S' => 'Š',
        's' => 'š',
        _ => ch,
    };
    output.push(accented);
    if ch.is_ascii_alphabetic() && matches!(ch.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u') {
        output.push(accented);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn fluent_transform_preserves_keys_placeholders_and_selectors() {
        let source =
            "# comment\ncount = { $count ->\n    [one] One item\n   *[other] { $count } items\n}\n";
        let bundle = build_bundle(source).expect("valid pseudo fixture");
        assert!(bundle.get_message("count").is_some(), "key is unchanged");

        let mut args = FluentArgs::new();
        args.set("count", 2);
        let message = bundle.get_message("count").expect("message");
        let mut errors = Vec::new();
        let rendered = bundle.format_pattern(
            message.value().expect("message value"),
            Some(&args),
            &mut errors,
        );
        assert!(
            errors.is_empty(),
            "selector syntax remains valid: {errors:?}"
        );
        assert!(rendered.contains('2'), "placeholder value is unchanged");
        assert!(!rendered.contains("items"), "message text is transformed");
    }

    #[test]
    fn output_is_non_ascii_and_expands_cell_width() {
        let canonical = "Workspace";
        let pseudo = pseudolocalize_value(canonical);
        assert!(!pseudo.is_ascii());
        assert!(UnicodeWidthStr::width(pseudo.as_str()) > UnicodeWidthStr::width(canonical));
    }

    #[test]
    fn bundle_interpolates_user_data_without_transforming_it() {
        let mut args = HashMap::new();
        args.insert(Cow::Borrowed("name"), FluentValue::from("plain-name"));
        let rendered = lookup("workspace-title", Some(&args)).expect("pseudo message");
        assert!(rendered.contains("plain-name"));
        assert!(!rendered.contains("plàïñ"));
    }
}
