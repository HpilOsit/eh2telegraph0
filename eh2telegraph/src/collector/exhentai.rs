use std::time::Duration;

use again::RetryPolicy;
use regex::Regex;
use reqwest::header;
use serde::Deserialize;

use crate::{
    config,
    http_proxy::ProxiedClient,
    stream::AsyncStream,
    util::match_first_group,
    util::{get_bytes, get_string},
};

use super::{
    utils::paged::{PageFormatter, PageIndicator, Paged},
    AlbumMeta, Collector, ImageData, ImageMeta,
};

lazy_static::lazy_static! {
    static ref PAGE_RE: Regex = Regex::new(r#"<a href="(https://exhentai\.org/s/\w+/[\w-]+)">"#).unwrap();
    static ref IMG_RE: Regex = Regex::new(r#"<img id="img" src="(.*?)""#).unwrap();
    static ref TITLE_RE: Regex = Regex::new(r#"<h1 id="gj">(.*?)</h1>"#).unwrap();

    static ref RETRY_POLICY: RetryPolicy = RetryPolicy::fixed(Duration::from_millis(200))
        .with_max_retries(5)
        .with_jitter(true);
}
const CONFIG_KEY: &str = "exhentai";
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct EXCollector {
    proxy_client: ProxiedClient,
    client: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct ExConfig {
    pub ipb_pass_hash: String,
    pub ipb_member_id: String,
    pub igneous: String,
}

impl EXCollector {
    pub fn new(config: &ExConfig, proxy_client: ProxiedClient) -> anyhow::Result<Self> {
        let cookie_value = format!(
            "ipb_pass_hash={};ipb_member_id={};igneous={};nw=1",
            config.ipb_pass_hash, config.ipb_member_id, config.igneous
        );

        // set headers with exhentai cookies
        let mut request_headers = header::HeaderMap::new();
        request_headers.insert(
            header::COOKIE,
            header::HeaderValue::from_str(&cookie_value)?,
        );
        Ok(Self {
            client: {
                reqwest::Client::builder()
                    。default_headers(request_headers.clone())
                    。timeout(TIMEOUT)
                    。build()
                    。expect("build reqwest client failed")
            },
            proxy_client: proxy_client.with_default_headers(request_headers),
        })
    }

    pub fn new_from_config() -> anyhow::Result<Self> {
        let config: ExConfig = config::parse(CONFIG_KEY)?
            。ok_or_else(|| anyhow::anyhow!("exhentai config(key: exhentai) not found"))?;
        let proxy_client = ProxiedClient::new_from_config();
        Self::new(&config, proxy_client)
    }

    pub fn get_client(&self) -> reqwest::Client {
        self.client.clone()
    }
}

impl Collector for EXCollector {
    type FetchError = anyhow::Error;
    type StreamError = anyhow::Error;
    type ImageStream = EXImageStream;

    #[inline]
    fn name() -> &'static str {
        "exhentai"
    }

    async fn fetch(
        &self,
        path: String,
    ) -> Result<(AlbumMeta, Self::ImageStream), Self::FetchError> {
        // normalize url
        let mut parts = path.trim_matches(|c| c == '/').split('/');
        let g = parts.next();
        let album_id = parts.next();
        let album_token = parts.next();
        let (album_id, album_token) = match (g, album_id, album_token) {
            (Some("g"), Some(album_id), Some(album_token)) => (album_id, album_token),
            _ => {
                return Err(anyhow::anyhow!("invalid input path({path}), gallery url is expected(like https://exhentai.org/g/2129939/01a6e086b9)"));
            }
        };
        let url = format!("https://exhentai.org/g/{album_id}/{album_token}");
        tracing::info!("[exhentai] process {url}");

        let mut paged = Paged::new(0, EXPageIndicator { base: url.clone() });
        let gallery_pages = paged.pages(&self.proxy_client).await.map_err(|e| {
            tracing::error!("[exhentai] load page failed: {e:?}");
            e
        })?;
        tracing::info!("[exhentai] pages loaded for {album_id}/{album_token}");

        // Since paged returns at least one page, we can safely get it.
        let title = match_first_group(&TITLE_RE, &gallery_pages[0])
            .unwrap_or("No Title")
            .to_string();

        let mut image_page_links = Vec::new();
        for gallery_page in gallery_pages.iter() {
            PAGE_RE.captures_iter(gallery_page).for_each(|c| {
                let matching = c.get(1).expect("regexp is matched but no group 1 found");
                image_page_links.push(matching.as_str().to_string());
            });
        }

        if image_page_links.is_empty() {
            return Err(anyhow::anyhow!(
                "invalid url, maybe resource has been deleted, or our ip is blocked."
            ));
        }

        Ok((
            AlbumMeta {
                link: url,
                name: title,
                class: None,
                description: None,
                authors: None,
                tags: None,
            },
            EXImageStream {
                client: self.client.clone(),
                proxy_client: self.proxy_client.clone(),
                image_page_links: image_page_links.into_iter(),
            },
        ))
    }
}

#[derive(Debug)]
pub struct EXImageStream {
    client: reqwest::Client,
    proxy_client: ProxiedClient,
    image_page_links: std::vec::IntoIter<String>,
}

impl EXImageStream {
    async fn load_image(
        proxy_client: ProxiedClient,
        client: reqwest::Client,
        link: String,
    ) -> anyhow::Result<(ImageMeta, ImageData)> {
        let content = RETRY_POLICY
            .retry(|| async { get_string(&proxy_client, &link).await })
            .await?;
        let img_url = match_first_group(&IMG_RE, &content)
            .ok_or_else(|| anyhow::anyhow!("unable to find image in page"))?;
        let image_data = RETRY_POLICY
            .retry(|| async { get_bytes(&client, img_url).await })
            .await?;

        tracing::trace!(
            "download exhentai image with size {}, link: {link}",
            image_data.len()
        );
        let meta = ImageMeta {
            id: link,
            url: img_url.to_string(),
            description: None,
        };
        Ok((meta, image_data))
    }
}

impl AsyncStream for EXImageStream {
    type Item = anyhow::Result<(ImageMeta, ImageData)>;

    type Future = impl std::future::Future<Output = Self::Item>;

    fn next(&mut self) -> Option<Self::Future> {
        let link = self.image_page_links.next()?;
        let client = self.client.clone();
        let proxy_client = self.proxy_client.clone();
        Some(async move { Self::load_image(proxy_client, client, link).await })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.image_page_links.size_hint()
    }
}

struct EXPageIndicator {
    base: String,
}

impl PageFormatter for EXPageIndicator {
    fn format_n(&self, n: usize) -> String {
        format!("{}/?p={}", self.base, n)
    }
}

impl PageIndicator for EXPageIndicator {
    fn is_last_page(&self, content: &str, next_page: usize) -> bool {
        let html = format!(
            "<a href=\"{}/?p={}\" onclick=\"return false\">",
            self.base, next_page
        );
        !content.contains(&html)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[ignore]
    #[tokio::test]
    async fn demo() {
        let config = ExConfig {
            ipb_pass_hash: "balabala".to_string(),
            ipb_member_id: "balabala".to_string(),
            igneous: "balabala".to_string(),
        };
        println!("config {config:#?}");
        let collector = EXCollector::new(&config, ProxiedClient::default()).unwrap();
        let (album, mut image_stream) = collector
            .fetch("/g/2129939/01a6e086b9".to_string())
            .await
            .unwrap();
        println!("album: {album:?}");

        let maybe_first_image = image_stream.next().unwrap().await;
        if let Ok((meta, data)) = maybe_first_image {
            println!("first image meta: {meta:?}");
            println!("first image data length: {}", data.len());
        }
    }

    #[ignore]
    #[tokio::test]
    async fn invalid_url() {
        let config = ExConfig {
            ipb_pass_hash: "balabala".to_string(),
            ipb_member_id: "balabala".to_string(),
            igneous: "balabala".to_string(),
        };
        println!("config {config:#?}");
        let collector = EXCollector::new(&config, ProxiedClient::default()).unwrap();
        let output = collector.fetch("/g/2129939/00000".to_string()).await;
        assert!(output.is_err());
        println!("output err {output:?}");
    }

    #[ignore]
    #[test]
    fn regex_match() {
        // test page: https://exhentai.org/g/2122174/fd2525031e
        let r = Regex::new(r#"<a href="(https://exhentai\.org/s/\w+/[\w-]+)">"#).unwrap();
        let h = r#"<div class="gdtm" style="height:170px"><div style="margin:1px auto 0; width:100px; height:140px; background:transparent url(https://ehgt.org/m/002122/2122174-00.jpg) -600px 0 no-repeat"><a href="https://exhentai.org/s/bd2b37d829/2122174-7"><img alt="007" title="Page 7: 2.png" src="https://ehgt.org/g/blank.gif" style="width:100px; height:139px; margin:-1px 0 0 -1px" /></a></div></div><div class="gdtm" style="height:170px"><div style="margin:1px auto 0; width:100px; height:100px; background:transparent url(https://ehgt.org/m/002122/2122174-00.jpg) -700px 0 no-repeat"><a href="https://exhentai.org/s/4ca72f757d/2122174-8"><img alt="008" title="Page 8: 3.png" src="https://ehgt.org/g/blank.gif" style="width:100px; height:99px; margin:-1px 0 0 -1px" />"#;

        let mut iter = r.captures_iter(h);
        let first = iter.next().unwrap();
        println!("{}", first.get(1).unwrap().as_str());

        let second = iter.next().unwrap();
        println!("{}", second.get(1).unwrap().as_str());
    }
}
