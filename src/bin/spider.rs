use clap::Parser;
use fjall::{Config, Keyspace, PartitionCreateOptions, PartitionHandle};
use jiff::Timestamp;
use reqwest::Proxy;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    error::Error,
    fs::create_dir_all,
    path::{Path, PathBuf},
    sync::LazyLock,
};
use tracing::{info, instrument};
use urlencoding::encode;
use whynot::{get_filename_from_url, kv_sep_partition_option, tag_key};

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let mut client_builder = reqwest::Client::builder();
    if let Some(proxy) = &ARGS.proxy {
        client_builder = client_builder.proxy(Proxy::all(proxy).unwrap());
    }
    client_builder
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap()
});

static ARGS: LazyLock<Args> = LazyLock::new(Args::parse);

/// whynot website crawler, downloading lists, pages and imgs
#[derive(Parser, Debug)]
struct Args {
    /// proxy (e.g., http://127.0.0.1:8089)
    #[clap(long)]
    proxy: Option<String>,
    #[arg(short = 'o', long, default_value = "wainao")]
    output: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    let path = Path::new(&ARGS.output);
    if !path.exists() {
        create_dir_all(path)?;
    }
    std::env::set_current_dir(path)?;

    let img_path = PathBuf::from("imgs");
    if !img_path.exists() {
        create_dir_all(img_path)?;
    }

    let keyspace = Config::new("wainao.db").open().unwrap();
    let db = keyspace
        .open_partition("wainao", kv_sep_partition_option())
        .unwrap();
    let index = keyspace
        .open_partition("index", PartitionCreateOptions::default())
        .unwrap();
    let tags = keyspace
        .open_partition("tags", PartitionCreateOptions::default())
        .unwrap();

    for i in ["/wainao-reads", "/english", "/wainao-watches"] {
        fetch_section(&keyspace, &db, &index, &tags, i).await;
    }
    Ok(())
}

async fn fetch_section(
    keyspace: &Keyspace,
    db: &PartitionHandle,
    index: &PartitionHandle,
    tags: &PartitionHandle,
    section: &str,
) {
    let mut offset = 0;
    let (count, items) = fetch_story_list(offset, section).await.unwrap();
    batch_dl(&items, keyspace, db, index, tags).await;

    offset += items.len();
    while offset < count {
        let (_, items) = fetch_story_list(offset, section).await.unwrap();
        batch_dl(&items, keyspace, db, index, tags).await;
        offset += items.len();
    }
}

const CDN_DOMAIN: &str = "https://cloudfront-us-east-1.images.arcpublishing.com/radiofreeasia/";

async fn batch_dl(
    items: &Vec<Value>,
    keyspace: &Keyspace,
    db: &PartitionHandle,
    index: &PartitionHandle,
    tags: &PartitionHandle,
) {
    let mut batch = keyspace.batch();
    for item in items {
        let mut imgs = HashSet::new();
        if let Some(img_url) = item["promo_items"]["basic"]["url"].as_str() {
            imgs.insert(img_url.to_owned());
        }

        let item_str = serde_json::to_string_pretty(item).unwrap();
        for line in item_str.lines() {
            if line.contains(CDN_DOMAIN) {
                let (_, img_name) = line.split_once(CDN_DOMAIN).unwrap();
                let img_name = img_name.trim().trim_end_matches("\",");
                let img_url = format!("{CDN_DOMAIN}{img_name}");
                imgs.insert(img_url);
            }
        }

        for img_url in imgs {
            let img_name = get_filename_from_url(&img_url);
            let img_path = PathBuf::from("imgs");
            let img_path = img_path.join(img_name);
            if !Path::new(&img_path).exists() {
                dl_obj(&img_url, &img_path).await.unwrap();
                info!("Downloaded image: {}", img_url);
            } else {
                info!("Image already exists: {}", img_path.display());
            }
        }

        let website_url = item["website_url"].as_str().unwrap().trim_matches('/');
        if !db.contains_key(website_url).unwrap() {
            let v = serde_json::to_string(&item).unwrap();

            let display_date = item["first_publish_date"].as_str().unwrap();
            let ts: Timestamp = display_date.parse().unwrap();
            let ts_byte = ts.as_second().to_be_bytes();

            let mut key = Vec::with_capacity(8 + website_url.len());
            key.extend_from_slice(&ts_byte);
            key.extend_from_slice(website_url.as_bytes());

            if let Some(sections) = item["taxonomy"]["sections"].as_array() {
                for section in sections {
                    let path = section["path"]
                        .as_str()
                        .unwrap()
                        .trim_matches('/')
                        .to_owned();
                    let key = tag_key(&path, &website_url, display_date);
                    batch.insert(tags, key, []);
                }
            }

            batch.insert(db, website_url, v);
            batch.insert(index, key, []);
        }
    }
    batch.commit().unwrap();
}

#[instrument]
async fn fetch_story_list(
    offset: usize,
    section: &str,
) -> Result<(usize, Vec<Value>), Box<dyn Error>> {
    let url = "https://www.wainao.me/pf/api/v3/content/fetch/story-feed-sections";
    let query_json = json!({
        "feedOffset": offset,
        "feedSize": 100,
        "includeSections": section
    });
    let query_json = query_json.to_string();
    let query = encode(&query_json);

    let url = format!("{url}?query={}&d=147&mxId=00000000&_website=wainao", query);
    let resp = CLIENT.get(url).send().await?;
    info!("Status: {}", resp.status());
    let text = resp.text().await?;
    let json: Value = serde_json::from_str(&text)?;
    let count = json["count"].as_u64().unwrap() as usize;
    Ok((
        count,
        json["content_elements"].as_array().unwrap().to_owned(),
    ))
}

#[instrument]
async fn dl_obj(url: &str, path: &Path) -> Result<(), reqwest::Error> {
    let resp = CLIENT.get(url).send().await?;
    info!("Status: {}", resp.status());

    let bytes = resp.bytes().await?;
    std::fs::write(path, &bytes).unwrap();
    Ok(())
}
