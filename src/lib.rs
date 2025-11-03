use fjall::{KvSeparationOptions, PartitionCreateOptions};
use jiff::Timestamp;

pub fn kv_sep_partition_option() -> PartitionCreateOptions {
    PartitionCreateOptions::default()
        .max_memtable_size(128_000_000)
        .with_kv_separation(
            KvSeparationOptions::default()
                .separation_threshold(750)
                .file_target_size(256_000_000),
        )
}

pub fn get_filename_from_url(url: &str) -> &str {
    url.split('/')
        .next_back()
        .and_then(|s| s.split('?').next())
        .unwrap()
}

/// tag_path + "|" + ts + website_url
pub fn tag_key(tag_path: &str, website_url: &str, display_date: &str) -> Vec<u8> {
    let ts: Timestamp = display_date.parse().unwrap();
    let ts_byte = ts.as_second().to_be_bytes();

    let mut key = Vec::with_capacity(tag_path.len() + 1 + 8 + website_url.len());

    key.extend_from_slice(tag_path.as_bytes());
    key.push(b'|');
    key.extend_from_slice(&ts_byte);
    key.extend_from_slice(website_url.as_bytes());

    key
}
