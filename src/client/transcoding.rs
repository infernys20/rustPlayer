use anyhow::Result;
use reqwest::{Client, Url};

use super::checked;
use crate::protocol::{PrepareRequest, Quality, RenditionInfo, RenditionStatus};

pub enum Operation {
    Prepare,
    Cancel,
    Retry,
}

pub async fn inventory(http: &Client, base: &Url, token: &str) -> Result<Vec<RenditionInfo>> {
    Ok(checked(
        http.get(base.join("api/renditions")?)
            .bearer_auth(token)
            .send()
            .await?,
    )
    .await?
    .json()
    .await?)
}

pub async fn operate(
    http: Client,
    base: Url,
    token: String,
    quality: Quality,
    operation: Operation,
) -> Result<Vec<RenditionInfo>> {
    let request = match operation {
        Operation::Prepare => http
            .post(base.join("api/prepare")?)
            .json(&PrepareRequest { quality }),
        Operation::Cancel => {
            http.post(base.join(&format!("api/renditions/{}/cancel", quality.key()))?)
        }
        Operation::Retry => {
            http.post(base.join(&format!("api/renditions/{}/retry", quality.key()))?)
        }
    };
    let _: RenditionStatus = checked(request.bearer_auth(&token).send().await?)
        .await?
        .json()
        .await?;
    inventory(&http, &base, &token).await
}
