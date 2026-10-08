//! Port of `pi-core/ai/src/utils/headers.ts`.

use std::collections::BTreeMap;

use crate::ai::types::ProviderHeaders;

/// Port of `headersToRecord` for the transport header representation.
pub fn headers_to_record(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers.iter().cloned().collect()
}

/// Port of `providerHeadersToRecord`: drops `null` (suppressed) headers and
/// returns `None` when nothing remains.
pub fn provider_headers_to_record(
    headers: Option<&ProviderHeaders>,
) -> Option<BTreeMap<String, String>> {
    let headers = headers?;
    let mut result: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        if let Some(value) = value {
            result.insert(name.clone(), value.clone());
        }
    }
    (!result.is_empty()).then_some(result)
}
