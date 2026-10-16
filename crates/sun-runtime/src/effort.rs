//! Shared effort-level pieces: the display label of each CLI effort token
//! (as the CLIs' own UIs spell it), the token check, and the tool-wide
//! union of a per-model catalog. Reading a CLI's own listing stays in its
//! adapter (`claude --help`, `codex debug models`).

use sun_core::{EffortLevel, ModelEffort};

/// Effort tokens across CLIs, lowest first, with the label the CLIs show.
const LABELS: &[(&str, &str)] = &[
    ("none", "None"),
    ("minimal", "Minimal"),
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
    ("ultra", "Ultra"),
];

/// `value` as an [`EffortLevel`], or `None` when it isn't a plain token
/// (it ends up as a CLI argument). A token Sun doesn't know yet keeps the
/// CLI's spelling, capitalized, so a new level shows up without a release.
pub fn level(value: &str, description: Option<String>) -> Option<EffortLevel> {
    let plain = !value.is_empty()
        && value.len() <= 32
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !plain {
        return None;
    }
    let label = match LABELS.iter().find(|(token, _)| *token == value) {
        Some((_, label)) => (*label).to_owned(),
        None => {
            let mut chars = value.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        }
    };
    Some(EffortLevel {
        value: value.to_owned(),
        label,
        description: description.filter(|d| !d.trim().is_empty()),
    })
}

/// Every level any of `models` accepts, lowest first; tokens Sun doesn't
/// know go last in the order first seen. Descriptions are per model, so
/// none here.
pub fn union(models: &[ModelEffort]) -> Vec<EffortLevel> {
    let mut values: Vec<&str> = Vec::new();
    for option in models.iter().flat_map(|m| &m.effort_options) {
        if !values.contains(&option.value.as_str()) {
            values.push(&option.value);
        }
    }
    let rank = |v: &str| {
        LABELS
            .iter()
            .position(|(t, _)| *t == v)
            .unwrap_or(LABELS.len())
    };
    // Stable: unknown tokens keep their first-seen order.
    values.sort_by_key(|v| rank(v));
    values.into_iter().filter_map(|v| level(v, None)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_tokens_and_unions_lowest_first() {
        let xhigh = level("xhigh", Some("Deeper".into())).unwrap();
        assert_eq!(
            (xhigh.label.as_str(), xhigh.description.as_deref()),
            ("Extra high", Some("Deeper"))
        );
        assert_eq!(level("turbo", None).unwrap().label, "Turbo");
        for bad in ["", "-x", "a b", "low;rm", &"a".repeat(33)] {
            assert_eq!(level(bad, None), None, "{bad}");
        }
        let model = |values: &[&str]| ModelEffort {
            id: "m".into(),
            label: "M".into(),
            effort_options: values.iter().filter_map(|v| level(v, None)).collect(),
            default_effort: None,
        };
        let all = union(&[
            model(&["high", "turbo", "low"]),
            model(&["minimal", "high", "zeta"]),
        ]);
        let values: Vec<&str> = all.iter().map(|l| l.value.as_str()).collect();
        assert_eq!(values, ["minimal", "low", "high", "turbo", "zeta"]);
    }
}
