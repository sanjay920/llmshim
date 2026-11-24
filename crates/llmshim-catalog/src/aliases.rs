//! Explicit transport spelling rules. Lookup aliases never rewrite a regional
//! model's wire id or change the provider selected by a caller.

pub const PROVIDER_ALIASES: &[(&str, &str)] = &[("google", "gemini")];
pub const REGION_PREFIXES: &[&str] = &["us.", "eu.", "global."];

pub(crate) fn provider_key(key: &str) -> &str {
    PROVIDER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map(|(_, canonical)| *canonical)
        .unwrap_or(key)
}

pub(crate) fn claude_version_spellings(name: &str) -> Vec<String> {
    if !name.starts_with("claude-") {
        return Vec::new();
    }
    let dashed = name.replace('.', "-");
    let mut dotted = String::new();
    let chars: Vec<char> = dashed.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let is_version_separator = *c == '-'
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(char::is_ascii_digit)
            && chars[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count()
                != 8;
        dotted.push(if is_version_separator { '.' } else { *c });
    }
    vec![dashed, dotted]
}
