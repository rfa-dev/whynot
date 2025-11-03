use askama::Template;
use axum::{
    Router, ServiceExt,
    body::Body,
    extract::{OriginalUri, Query, Request, State},
    http::{Response, Uri, header},
    response::{Html, IntoResponse},
    routing::get,
};
use clap::Parser;
use fjall::{Config, PartitionCreateOptions, PartitionHandle};
use jiff::{Timestamp, tz::TimeZone};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{net::SocketAddr, path::PathBuf, sync::LazyLock};
use tokio::net::TcpListener;
use tower::Layer;
use tower_http::{normalize_path::NormalizePathLayer, services::ServeDir};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use whynot::{get_filename_from_url, kv_sep_partition_option};

/// RFA backup website
#[derive(Parser, Debug)]
struct Args {
    /// listening address
    #[arg(short, long, default_value = "127.0.0.1:3334")]
    addr: String,

    /// data folder, containing imgs/ and whynot.db/
    #[arg(short = 'd', long, default_value = "whynot_data")]
    data: String,
}

static ARGS: LazyLock<Args> = LazyLock::new(Args::parse);

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let folder = PathBuf::from(&ARGS.data);
    let db_folder = folder.join("whynot.db");

    let keyspace = Config::new(db_folder).open().unwrap();
    let db = keyspace
        .open_partition("whynot", kv_sep_partition_option())
        .unwrap();
    let index = keyspace
        .open_partition("index", PartitionCreateOptions::default())
        .unwrap();
    let tags = keyspace
        .open_partition("tags", PartitionCreateOptions::default())
        .unwrap();
    let app_state = AppState { db, index, tags };

    let addr: SocketAddr = ARGS.addr.parse().unwrap();
    info!("Listening to {addr}");

    let img_folder = folder.join("imgs");
    let app = Router::new()
        .route("/", get(list))
        .route("/{*id}", get(page))
        .route("/style.css", get(style))
        .route("/favicon.ico", get(favicon))
        .nest_service("/imgs", ServeDir::new(img_folder))
        .with_state(app_state)
        .fallback(handler_404);
    let app = NormalizePathLayer::trim_trailing_slash().layer(app);

    let listener = TcpListener::bind(addr).await.unwrap();

    axum::serve(listener, ServiceExt::<Request>::into_make_service(app))
        .await
        .unwrap();
}

async fn page(
    Query(params): Query<SiteParams>,
    State(state): State<AppState>,
    OriginalUri(original_uri): OriginalUri,
) -> impl IntoResponse {
    let original_uri = original_uri.to_string();
    let key = original_uri.split("?").next().unwrap().trim_matches('/');
    if let Some(v) = state.db.get(key).unwrap() {
        info!("page: {key}");
        let json: Value = serde_json::from_slice(&v).unwrap();
        let article: Article = (&json).into();
        into_response(&article)
    } else {
        let page = params.page.unwrap_or_default();
        info!("page: {key}, page:{page}");
        let n = page * 20;
        let len = key.len() + 1;
        let mut prefix = Vec::with_capacity(len);
        prefix.extend_from_slice(key.as_bytes());
        prefix.push(b'|');

        let mut items = Vec::with_capacity(20);
        for (idx, i) in state.tags.prefix(prefix).rev().enumerate() {
            if idx < n {
                continue;
            }
            if idx >= n + 20 {
                break;
            }
            let (k, _) = i.unwrap();
            let website_key = &k[len + 8..];
            let v2 = state.db.get(&website_key).unwrap().unwrap();
            let json: Value = serde_json::from_slice(&v2).unwrap();
            let item: Item = (&json).into();
            items.push(item);
        }

        let url_path = format!("/{key}");
        let page_list = PageList {
            items,
            page,
            url_path,
        };
        into_response(&page_list)
    }
}

#[derive(Deserialize)]
struct SiteParams {
    page: Option<usize>,
}

#[derive(Debug, Serialize)]
enum ContentType {
    Text(String),
    Image(String, String),
    Header(String),
    Link(String, String),
    RawHtml(String),
    Quote(String),
    CustomEmbed(String, String),
    #[allow(dead_code)]
    Other,
}

#[derive(Template, Debug, Serialize)]
#[template(path = "article.html", escape = "none")]
struct Article {
    site: String,
    item: Item,
    author: Option<String>,
    contents: Vec<ContentType>,
    topics: Vec<(String, String)>,
    tags: Vec<(String, String)>,
}

impl From<&Value> for Article {
    fn from(json: &Value) -> Self {
        let item: Item = json.into();
        let site = item
            .website_url
            .trim_start_matches('/')
            .split_once('/')
            .unwrap()
            .0
            .to_owned();
        let author = json
            .get("credits")
            .and_then(|p| p.get("by"))
            .and_then(|b| b.as_array())
            .and_then(|a| a.first())
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .map(|s| s.to_owned());

        let mut contents = vec![];
        let mut has_article = false;
        if let Some(content_elements) = json["content_elements"].as_array() {
            for c in content_elements {
                match c["type"].as_str().unwrap() {
                    "text" => {
                        let content = c["content"].as_str().unwrap();
                        if !content.is_empty() {
                            contents.push(ContentType::Text(content.to_owned()))
                        }
                    }
                    "image" => {
                        let url = c["url"].as_str().unwrap();
                        let img_name = get_filename_from_url(url);
                        let url = format!("/imgs/{img_name}");
                        let caption = c["caption"].as_str().unwrap_or_default();
                        contents.push(ContentType::Image(url, caption.to_owned()))
                    }
                    "header" => {
                        let content = c["content"].as_str().unwrap();
                        if !content.is_empty() {
                            contents.push(ContentType::Header(content.to_owned()))
                        }
                    }
                    "interstitial_link" => {
                        let url = c["url"]
                            .as_str()
                            .unwrap()
                            .replace("https://www.rfa.org", "");
                        let content = if let Some(content) = c["content"].as_str() {
                            content.to_owned()
                        } else {
                            url.clone()
                        };
                        contents.push(ContentType::Link(content, url));
                    }
                    "raw_html" => {
                        if !has_article {
                            let content = c["content"]
                                .as_str()
                                .unwrap()
                                .trim_start_matches("<noscript>")
                                .trim_end_matches("</noscript>")
                                .trim();
                            if !content.is_empty() {
                                contents.push(ContentType::RawHtml(content.to_owned()))
                            }
                        }
                    }
                    "quote" => {
                        let mut content;
                        if let Some(content_elements) = c["content_elements"].as_array() {
                            for i in content_elements {
                                if let Some(i_type) = i["type"].as_str() {
                                    if i_type == "text" {
                                        content =
                                            i["content"].as_str().unwrap_or_default().to_owned();
                                        if !content.is_empty() {
                                            contents.push(ContentType::Quote(content))
                                        } else {
                                            warn!(
                                                "{} -> unknown content type: {c}",
                                                item.website_url
                                            )
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "custom_embed" => {
                        let mut url = String::new();
                        let mut content = String::new();
                        if let Some(config) = c["embed"]["config"].as_object() {
                            if let Some(u) = config.get("shorthandScript") {
                                url = u.as_str().unwrap().to_owned();
                            } else if let Some(u) = config.get("url") {
                                url = u.as_str().unwrap().to_owned();
                            }
                            if let Some(c) = c.get("article") {
                                content = c.as_str().unwrap().to_owned();
                                has_article = true;
                            }
                        }
                        contents.push(ContentType::CustomEmbed(url, content));
                    }
                    _ => {
                        warn!("{} -> unknown content type: {c}", item.website_url)
                    }
                }
            }
        }

        let mut topics = vec![];
        let mut tags = vec![];

        if let Some(sections) = json["taxonomy"]["sections"].as_array() {
            for section in sections {
                let path = section["path"].as_str().unwrap().to_owned();
                let name = section["name"].as_str().unwrap().to_owned();
                if path.starts_with("/topics") {
                    topics.push((path, name));
                } else if path.starts_with("/tags") {
                    tags.push((path, name));
                }
            }
        }

        Self {
            site,
            item,
            author,
            contents,
            topics,
            tags,
        }
    }
}

async fn list(
    Query(params): Query<SiteParams>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let index = state.index;
    let db = state.db;
    let mut items = Vec::with_capacity(20);
    let page = params.page.unwrap_or(0);
    let n = page * 20;
    for (idx, i) in index.iter().rev().enumerate() {
        if idx < n {
            continue;
        }
        if idx >= n + 20 {
            break;
        }
        let (k, _) = i.unwrap();
        let db_key = &k[8..];
        if let Some(v) = db.get(db_key).unwrap() {
            let json: Value = serde_json::from_slice(&v).unwrap();
            let item: Item = (&json).into();
            items.push(item)
        }
    }

    let url_path = format!("/");
    let page_list = PageList {
        items,
        page: page + 1,
        url_path,
    };
    into_response(&page_list)
}

async fn handler_404(uri: Uri) -> impl IntoResponse {
    error!("No route for {}", uri);
    (
        StatusCode::NOT_FOUND,
        Html("404 NOT FOUND.<br>Back to <a href='/'>Home</a>"),
    )
}

#[derive(Clone)]
struct AppState {
    db: PartitionHandle,
    index: PartitionHandle,
    tags: PartitionHandle,
}

#[derive(Debug, Serialize)]
struct Item {
    headlines: String,
    display_date: String,
    description: String,
    promo_img: Option<String>,
    caption: Option<String>,
    website_url: String,
    section: (String, String),
}

impl From<&Value> for Item {
    fn from(json: &Value) -> Self {
        let headlines = json["headlines"]["basic"].as_str().unwrap().to_owned();
        let display_date = json["publish_date"].as_str().unwrap();
        let ts: Timestamp = display_date.parse().unwrap();
        let display_date = ts.to_zoned(TimeZone::UTC).strftime("%Y-%m-%d").to_string();

        let description = json["description"]["basic"].as_str().unwrap().to_owned();

        let promo_img = json
            .get("promo_items")
            .and_then(|p| p.get("basic"))
            .and_then(|b| b.get("url"))
            .and_then(|img| img.as_str())
            .map(|s| {
                let img_name = get_filename_from_url(s);
                format!("/imgs/{img_name}")
            });

        let caption = json
            .get("promo_items")
            .and_then(|p| p.get("basic"))
            .and_then(|b| b.get("caption"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_owned());

        let mut id = String::new();
        let mut name = String::new();
        let mut website_url = String::new();
        if let Some(obj) = json["websites"].as_object()
            && let Some((_, value)) = obj.iter().next()
        {
            website_url = value["website_url"].as_str().unwrap().to_owned();
            let section = value.get("website_section").unwrap();
            id = section
                .get("_id")
                .unwrap_or_default()
                .as_str()
                .unwrap_or_default()
                .to_owned();
            name = section
                .get("name")
                .unwrap_or_default()
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        Item {
            headlines,
            display_date,
            description,
            promo_img,
            caption,
            website_url,
            section: (id, name),
        }
    }
}

#[derive(Template)]
#[template(path = "list.html")]
struct PageList {
    items: Vec<Item>,
    page: usize,
    url_path: String,
}

fn into_response<T: Template>(t: &T) -> Response<Body> {
    match t.render() {
        Ok(body) => Html(body).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn style() -> impl IntoResponse {
    let headers = [
        (header::CONTENT_TYPE, "text/css"),
        (
            header::CACHE_CONTROL,
            "public, max-age=1209600, s-maxage=86400",
        ),
    ];

    (headers, include_str!("../../static/style.css"))
}

async fn favicon() -> impl IntoResponse {
    let headers = [
        (header::CONTENT_TYPE, "image/x-icon"),
        (
            header::CACHE_CONTROL,
            "public, max-age=1209600, s-maxage=86400",
        ),
    ];

    (headers, include_bytes!("../../static/favicon.ico"))
}
