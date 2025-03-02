use fixed_decimal::{FixedDecimal, FloatPrecision};
use icu_decimal::FixedDecimalFormatter;
use icu_locid::locale;
use minijinja::Value;

/// This filter will format the number with thousand separators and two decimal places.
pub fn currency_format(value: f64, lang: Value, magnitude: Option<u8>) -> String {
    let magnitude = magnitude.unwrap_or(2) as i16;

    let locale = match lang.as_str() {
        Some("de") => locale!("de-DE"),
        Some("en") => locale!("en-US"),
        _ => locale!("en-US"),
    };

    let fdf = FixedDecimalFormatter::try_new(&locale.into(), Default::default())
        .expect("locale should be present");

    // this caps the number to `.XX`!
    // note, that using FloatPrecision::Floating ("infinite" precision) will misformat e.g.
    // `0.00` as `0`, which is not what's expected.
    let fixed_decimal = FixedDecimal::try_from_f64(value, FloatPrecision::Magnitude(-magnitude))
        .expect("cannot get decimal from float");

    fdf.format_to_string(&fixed_decimal)
}

/// This filter is just a small wrapper around str::split
pub fn split(input: &str, pat: &str) -> Vec<String> {
    input.split(pat).map(str::to_string).collect()
}

/// This will tex-escape all characters but `\`.
pub fn context_escape(input: &str) -> String {
    input
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('#', "\\letterhash{}")
        .replace('$', "\\letterdollar{}")
        .replace('%', "\\letterpercent{}")
        .replace('&', "\\letterampersand{}")
        .replace('_', "\\letterunderscore{}")
        .replace('[', "\\letterleftbracket{}")
        .replace(']', "\\letterrightbracket{}")
        .replace('|', "\\letterbar{}")
        .replace('~', "\\lettertilde{}")
        .replace('^', "\\letterhat{}")
}
