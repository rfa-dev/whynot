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
    #[arg(short = 'o', long, default_value = "whynot_data")]
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

    let keyspace = Config::new("whynot.db").open().unwrap();
    let db = keyspace
        .open_partition("whynot", kv_sep_partition_option())
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
    let (count, mut items) = fetch_story_list(offset, section).await.unwrap();
    batch_dl(&mut items, keyspace, db, index, tags).await;

    offset += items.len();
    while offset < count {
        let (_, mut items) = fetch_story_list(offset, section).await.unwrap();
        batch_dl(&mut items, keyspace, db, index, tags).await;
        offset += items.len();
    }
}

const CDN_DOMAIN: &str = "https://cloudfront-us-east-1.images.arcpublishing.com/radiofreeasia/";

async fn batch_dl(
    items: &mut Vec<Value>,
    keyspace: &Keyspace,
    db: &PartitionHandle,
    index: &PartitionHandle,
    tags: &PartitionHandle,
) {
    let mut batch = keyspace.batch();
    for item in items.iter_mut() {
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

        if let Some(content_elements) = item["content_elements"].as_array_mut() {
            for c in content_elements.iter_mut() {
                if c["type"].as_str().unwrap() == "custom_embed" {
                    let mut url = String::new();
                    if let Some(config) = c["embed"]["config"].as_object() {
                        if let Some(u) = config.get("shorthandScript") {
                            url = u.as_str().unwrap().to_owned();
                        } else if let Some(u) = config.get("url") {
                            url = u.as_str().unwrap().to_owned();
                        }
                        if url.is_empty() {
                            continue;
                        }
                        let (article, img_urls) = extract_article(&url).await;
                        for (img_url, img_path) in img_urls {
                            if !Path::new(&img_path).exists() {
                                dl_obj(&img_url, &img_path).await.unwrap();
                                info!("Downloaded image: {}", img_url);
                            } else {
                                info!("Image already exists: {}", img_path.display());
                            }
                        }

                        if !article.is_empty() {
                            c.as_object_mut()
                                .unwrap()
                                .insert("article".to_owned(), Value::String(article));
                        }
                    }
                }
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

#[instrument]
async fn extract_article(web_url: &str) -> (String, Vec<(String, PathBuf)>) {
    let resp = CLIENT.get(web_url).send().await.unwrap();
    info!("Status: {}", resp.status());
    let html = resp.text().await.unwrap();
    let document = scraper::Html::parse_document(&html);
    let selector = scraper::Selector::parse(
        "h2.Theme-Layer-BodyText-Heading-Large, 
        div.Theme-Caption.Layout,
        picture,
        p",
    )
    .unwrap();
    let source_selector = &scraper::Selector::parse("source").unwrap();
    let caption_selector = scraper::Selector::parse(".Theme-Caption.Layout").unwrap();
    let caption_nodes: Vec<_> = document.select(&caption_selector).collect();

    let mut article = String::new();
    let mut img_urls = Vec::new();
    for element in document.select(&selector) {
        if element.value().name() == "picture" {
            let mut urls = HashSet::new();
            for source in element.select(source_selector) {
                if let Some(srcset) = source.value().attr("data-srcset") {
                    let img_url = srcset
                        .split(',')
                        .map(|s| s.trim())
                        .last()
                        .and_then(|s| s.split_whitespace().next())
                        .unwrap();
                    urls.insert(img_url);
                }
            }

            for url in urls {
                if url.ends_with("webp") {
                    continue;
                }
                let img_prefix = web_url.trim_end_matches("index.html");
                let i = url.trim_start_matches("./");
                let img_url = format!("{img_prefix}{i}");
                let mut img_name = url.trim_start_matches("./assets/").replace('/', "_");
                if img_name.len() > 200 {
                    img_name = img_name.split_at(100).1.to_owned();
                }
                let img_path = PathBuf::from("imgs");
                let img_path = img_path.join(img_name);
                article.push_str(&format!("<img src=\"/{}\" />\n", img_path.display()));
                img_urls.push((img_url, img_path));
                break;
            }
        } else if element.value().name() == "div" {
            if let Some(caption) = element
                .select(&scraper::Selector::parse("div.Theme-Caption.Layout > div").unwrap())
                .next()
            {
                let caption_html = caption.text().collect::<Vec<_>>().join("");
                article.push_str(&format!("<div class=\"caption\">{}</div>\n", caption_html));
            }
        } else if element.value().name() == "p" {
            let p = element.inner_html();
            if caption_nodes
                .iter()
                .any(|cap| cap.text().any(|_| cap.html().contains(&p)))
            {
                continue;
            }
            article.push_str(&format!("<p>{}</p>\n", p));
        } else {
            let html_fragment = element.html();
            article.push_str(&html_fragment);
            article.push_str("\n");
        }
    }

    (article, img_urls)
}
