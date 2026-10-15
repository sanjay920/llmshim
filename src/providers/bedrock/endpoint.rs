//! Supported AWS partitions use the standard regional Bedrock Runtime hostname.
const PARTITIONS: &[(&str, &str, &str)] = &[
    ("aws-cn", "cn-", "amazonaws.com.cn"),
    ("aws-us-gov", "us-gov-", "amazonaws.com"),
    ("aws", "", "amazonaws.com"),
];

pub(super) fn url(region: &str) -> Result<String, &'static str> {
    if region.is_empty()
        || !region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("Bedrock region must contain only lowercase letters, digits and hyphens");
    }
    for (prefix, name) in [
        ("us-iso-", "aws-iso"),
        ("us-isob-", "aws-iso-b"),
        ("eu-isoe-", "aws-iso-e"),
        ("us-isof-", "aws-iso-f"),
        ("eusc-", "aws-eusc"),
    ] {
        if region.starts_with(prefix) {
            return Err(name);
        }
    }
    let suffix = PARTITIONS
        .iter()
        .find(|(_, prefix, _)| region.starts_with(prefix))
        .map(|(_, _, suffix)| *suffix)
        .unwrap_or("amazonaws.com");
    Ok(format!("https://bedrock-runtime.{region}.{suffix}"))
}
