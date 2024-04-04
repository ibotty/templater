use fixed_decimal::{FixedDecimal, FloatPrecision};
use icu_decimal::FixedDecimalFormatter;
use icu_locid::locale;
use minijinja::{State, Value};

/// This filter will format the number with thousand separators and two decimal places.
/// If no language code is given, it will look up the `document_language` key in the state.
pub fn currency_format(state: &State, value: f64, lang: Option<Value>) -> String {
    let lang = lang.unwrap_or_else(|| {
        state
            .lookup("document_language")
            .expect("cannot find document_language key")
    });

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
    let fixed_decimal = FixedDecimal::try_from_f64(value, FloatPrecision::Magnitude(-2))
        .expect("cannot get decimal from float");

    fdf.format_to_string(&fixed_decimal)
}
