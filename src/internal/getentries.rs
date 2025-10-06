use std::convert::TryFrom;
use std::ops::Range;

use async_stream::try_stream;
use futures::Stream;

use crate::jsons;
use crate::Error;

use super::get_json;
use super::Leaf;

/// Request leaf entries from the CT log. Does not verify if these entries are
/// consistent with the tree or anything like that. Returns an iterator over the
/// leaves.
///
/// After the first Err result, the iterator will not produce anything else.
///
/// Uses `O(1)` memory itself.
pub fn get_entries<'a>(
    client: &'a reqwest::Client,
    base_url: &'a reqwest::Url,
    range: Range<u64>,
    batch_size: u64,
) -> impl Stream<Item = Result<Leaf, Error>> + 'a {
    try_stream! {
        let mut next_index = range.start;

        while next_index < range.end {
            let end = u64::min(next_index + batch_size, range.end);
            let url = format!("ct/v1/get-entries?start={}&end={}", next_index, end - 1);

            let entries: jsons::GetEntries = get_json(client, base_url, &url).await?;
            if entries.entries.is_empty() {
                break;
            }

            for entry in entries.entries {
                let leaf = Leaf::try_from(&entry)?;
                yield leaf;
            }

            next_index = end;
        }
    }
}
