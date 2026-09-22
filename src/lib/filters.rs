use icu_decimal::DecimalFormatter;
use icu_decimal::input::{Decimal, FloatPrecision};
use icu_locale_core::locale;
use minijinja::Value;
use qrcodegen::{QrCode, QrCodeEcc};

/// This filter will format the number with thousand separators and two decimal places.
pub fn currency_format(value: f64, lang: Value, magnitude: Option<u8>) -> String {
    let magnitude = magnitude.unwrap_or(2) as i16;

    let locale = match lang.as_str() {
        Some("de") => locale!("de-DE"),
        Some("en") => locale!("en-US"),
        _ => locale!("en-US"),
    };

    let fdf = DecimalFormatter::try_new(locale.into(), Default::default())
        .expect("locale should be present");

    // this caps the number to `.XX`!
    // note, that using FloatPrecision::Floating ("infinite" precision) will misformat e.g.
    // `0.00` as `0`, which is not what's expected.
    let fixed_decimal = Decimal::try_from_f64(value, FloatPrecision::Magnitude(-magnitude))
        .expect("cannot get decimal from float");

    fdf.format_to_string(&fixed_decimal)
}

/// This filter is just a small wrapper around str::split
pub fn split(input: &str, pat: &str) -> Vec<String> {
    input.split(pat).map(str::to_string).collect()
}

/// Encode `input` as a QR code and emit a MetaPost figure drawing it as filled
/// unit squares (one per dark module).
pub fn qr_encode_to_mp_picture(input: &str) -> String {
    let qr =
        QrCode::encode_text(input, QrCodeEcc::Medium).expect("input too long to encode as QR code");
    let n = qr.size();
    let mut out = String::from("beginfig(1);\n");
    for r in 0..n {
        for c in 0..n {
            if qr.get_module(c, r) {
                // flip so row 0 (QR top) is on top
                let (x, y) = (c, n - 1 - r);
                out += &format!("  fill unitsquare shifted ({x},{y});\n");
            }
        }
    }
    // out += "  currentpicture := currentpicture scaled u;\n";
    out += "endfig;\n";
    out
}

/// TeX-escapes every ConTeXt special character, including `\` (so untrusted
/// input cannot start a control sequence). Single pass: chained `str::replace`
/// cannot do this correctly because the inserted `\letter...{}` macros contain
/// braces that a later brace-replace would re-escape.
pub fn context_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\\' => out.push_str("\\letterbackslash{}"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '#' => out.push_str("\\letterhash{}"),
            '$' => out.push_str("\\letterdollar{}"),
            '%' => out.push_str("\\letterpercent{}"),
            '&' => out.push_str("\\letterampersand{}"),
            '_' => out.push_str("\\letterunderscore{}"),
            '[' => out.push_str("\\letterleftbracket{}"),
            ']' => out.push_str("\\letterrightbracket{}"),
            '|' => out.push_str("\\letterbar{}"),
            '~' => out.push_str("\\lettertilde{}"),
            '^' => out.push_str("\\letterhat{}"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes a raw URL for use as a `| safe` argument to ConTeXt's `\useURL`.
/// `\useURL` captures its value with `\detokenize`, which always doubles a
/// default-catcode-6 `#` (e.g. an anchor: `#section` becomes `##section`),
/// and `%` starts a TeX comment that would silently truncate the rest of
/// the line. Prefixing any TeX-special character with `\` sidesteps both:
/// `\X` isn't catcode 6, so it survives `\detokenize` untouched, and
/// ConTeXt's own url "untex" cleanup strips the backslash back off,
/// restoring the single literal character (verified against `context`
/// directly: `\$\&\_\^\~\{\}\#\%` round-trips to `$&_^~{}#%`).
/// `\` itself isn't escaped: a raw backslash isn't valid unencoded URL
/// syntax, and escaping it here would need a real tokenizer to keep the
/// other `\X` escapes below from re-matching it.
pub fn url_escape(input: &str) -> Value {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(
            c,
            '#' | '%' | '$' | '&' | '_' | '^' | '~' | '{' | '}' | '[' | ']'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    Value::from_safe_string(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_escape() {
        // backslash cannot start a control sequence
        let escaped = context_escape(r"\input x");
        assert!(!escaped.contains(r"\input"));
        assert_eq!(escaped, r"\letterbackslash{}input x");
        // braces from real input are escaped; macro braces are not re-escaped
        assert_eq!(context_escape("\\{"), "\\letterbackslash{}\\{");
        // plain text is untouched
        assert_eq!(context_escape("hello"), "hello");
    }

    #[test]
    fn test_url_escape() {
        assert_eq!(
            url_escape("https://example.com/page#section").to_string(),
            r"https://example.com/page\#section"
        );
        assert_eq!(url_escape("a%20b").to_string(), r"a\%20b");
        assert_eq!(url_escape("$&_^~{}[]x").to_string(), r"\$\&\_\^\~\{\}\[\]x");
        assert_eq!(
            url_escape("https://example.com").to_string(),
            "https://example.com"
        );
        assert!(url_escape("x").is_safe());
    }

    #[test]
    fn test_qr_encode_to_metapost_picture() {
        let mp = qr_encode_to_mp_picture("https://example.com");
        assert!(mp.starts_with("beginfig(1);\n"));
        assert!(mp.trim_end().ends_with("endfig;"));
        // top-left finder module (QR r=0,c=0) is dark and must map to the top row.
        assert!(mp.contains("fill unitsquare shifted (0,"));
    }
}
