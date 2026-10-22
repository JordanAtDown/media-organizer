use crate::error::NamingError;
use chrono::{DateTime, Local};
use std::path::{Component, Path, PathBuf};

/// Tokens a pattern may use.
const TOKENS: &[&str] = &[
    "year", "month", "day", "hour", "min", "sec", "stem", "ext", "camera", "prefix",
];

/// Everything a pattern can substitute.
pub struct PatternInput<'a> {
    pub date: &'a DateTime<Local>,
    /// File name without extension.
    pub stem: &'a str,
    /// Extension with its leading dot (`.jpg`), or empty.
    pub ext: &'a str,
    /// Sanitized camera model; `None` renders as `unknown`.
    pub camera: Option<&'a str>,
    /// `photo_prefix` or `video_prefix`, depending on the media type.
    pub prefix: &'a str,
}

/// Check a pattern at configuration time, so a typo fails `validate-config`
/// instead of producing a literal `{yeer}` directory at 3 a.m.
pub fn validate_pattern(pattern: &str) -> Result<(), NamingError> {
    if pattern.is_empty() {
        return Err(NamingError::InvalidPattern("pattern is empty".into()));
    }
    if pattern.starts_with('/') {
        return Err(NamingError::InvalidPattern(
            "pattern must be relative to output".into(),
        ));
    }
    if Path::new(pattern)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(NamingError::InvalidPattern(
            "pattern must not contain '..'".into(),
        ));
    }

    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| NamingError::InvalidPattern("unclosed '{'".into()))?;
        let token = &after[..close];
        if token == "counter" {
            return Err(NamingError::InvalidToken(
                "{counter} n'existe plus — utilisez on_conflict = \"rename\" \
                 pour gérer les collisions de noms"
                    .into(),
            ));
        }
        if !TOKENS.contains(&token) {
            return Err(NamingError::InvalidToken(format!("{{{token}}}")));
        }
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(NamingError::InvalidPattern("unmatched '}'".into()));
    }
    Ok(())
}

/// Render a pattern into a destination path relative to `output`.
///
/// One left-to-right pass: a substituted value is never re-scanned, so a file
/// literally named `{ext}.jpg` cannot inject a token.
pub fn apply_pattern(pattern: &str, input: &PatternInput) -> String {
    let mut out = String::with_capacity(pattern.len() + 32);
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                match render_token(&after[..close], input) {
                    Some(value) => out.push_str(&value),
                    // Unknown token: keep it verbatim (validation rejects it earlier).
                    None => {
                        out.push('{');
                        out.push_str(&after[..close]);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn render_token(token: &str, i: &PatternInput) -> Option<String> {
    let date = |fmt: &str| i.date.format(fmt).to_string();
    Some(match token {
        "year" => date("%Y"),
        "month" => date("%m"),
        "day" => date("%d"),
        "hour" => date("%H"),
        "min" => date("%M"),
        "sec" => date("%S"),
        "stem" => i.stem.to_string(),
        "ext" => i.ext.to_string(),
        "camera" => i.camera.unwrap_or("unknown").to_string(),
        "prefix" => i.prefix.to_string(),
        _ => return None,
    })
}

/// `photo.jpg` → `photo_3.jpg`: the name tried for the n-th conflicting file.
pub fn with_suffix(path: &Path, n: u32) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let name = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}_{n}.{ext}"),
        None => format!("{stem}_{n}"),
    };
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rstest::rstest;

    fn render(pattern: &str, camera: Option<&str>) -> String {
        let date = Local
            .with_ymd_and_hms(2024, 3, 5, 7, 8, 9)
            .single()
            .unwrap();
        apply_pattern(
            pattern,
            &PatternInput {
                date: &date,
                stem: "IMG_0001",
                ext: ".jpg",
                camera,
                prefix: "PHOTO_",
            },
        )
    }

    #[test]
    fn default_pattern_renders() {
        assert_eq!(
            render(
                "{year}/{month}/{year}-{month}-{day}_{hour}{min}{sec}_{stem}{ext}",
                None
            ),
            "2024/03/2024-03-05_070809_IMG_0001.jpg"
        );
    }

    #[test]
    fn camera_and_prefix_render() {
        assert_eq!(
            render("{camera}/{prefix}{stem}{ext}", Some("Pixel 7")),
            "Pixel 7/PHOTO_IMG_0001.jpg"
        );
        assert_eq!(render("{camera}", None), "unknown");
    }

    #[test]
    fn substituted_values_are_not_rescanned() {
        let date = Local
            .with_ymd_and_hms(2024, 3, 5, 7, 8, 9)
            .single()
            .unwrap();
        let out = apply_pattern(
            "{stem}{ext}",
            &PatternInput {
                date: &date,
                stem: "{year}",
                ext: ".jpg",
                camera: None,
                prefix: "",
            },
        );
        assert_eq!(out, "{year}.jpg");
    }

    #[rstest]
    #[case("{year}/{month}/{stem}{ext}")]
    #[case("{camera}/{prefix}{stem}{ext}")]
    #[case("static/name.jpg")]
    fn valid_patterns(#[case] p: &str) {
        assert!(validate_pattern(p).is_ok());
    }

    #[rstest]
    #[case("")]
    #[case("/abs/{stem}{ext}")]
    #[case("../{stem}{ext}")]
    #[case("{year}/../x")]
    #[case("{yeer}/{stem}")]
    #[case("{year")]
    #[case("year}")]
    fn invalid_patterns(#[case] p: &str) {
        assert!(validate_pattern(p).is_err(), "accepted: {p}");
    }

    #[test]
    fn counter_token_is_rejected_with_a_helpful_message() {
        let err = validate_pattern("{stem}_{counter}{ext}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("{counter} n'existe plus"), "{err}");
        assert!(err.contains("on_conflict"), "{err}");
    }

    #[rstest]
    #[case("out/a.jpg", 1, "out/a_1.jpg")]
    #[case("out/a.tar.gz", 2, "out/a.tar_2.gz")]
    #[case("out/noext", 3, "out/noext_3")]
    fn suffixes(#[case] path: &str, #[case] n: u32, #[case] expected: &str) {
        assert_eq!(with_suffix(Path::new(path), n), PathBuf::from(expected));
    }
}
