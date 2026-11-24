//! Supported AWS partitions use the standard regional Bedrock Runtime hostname.
const PARTITIONS: &[(&str, &str, &str)] = &[
    ("aws-cn", "cn-", "amazonaws.com.cn"),
    ("aws-us-gov", "us-gov-", "amazonaws.com"),
    ("aws", "", "amazonaws.com"),
];

pub(super) fn url(region: &str) -> Result<String, &'static str> {
    crate::providers::cloud_rules::bedrock_region(region)?;
    let suffix = PARTITIONS
        .iter()
        .find(|(_, prefix, _)| region.starts_with(prefix))
        .map(|(_, _, suffix)| *suffix)
        .unwrap_or("amazonaws.com");
    Ok(format!("https://bedrock-runtime.{region}.{suffix}"))
}
