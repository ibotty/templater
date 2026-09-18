use anyhow::{anyhow, bail, ensure};
use async_tempfile::TempFile;
use foundations::telemetry::log::{debug, trace};
use md5::{Digest, Md5};
use mime_guess::Mime;
use reqwest::{
    IntoUrl,
    header::{self, ETAG},
};
use tokio::io::AsyncReadExt;

pub async fn upload_file(
    client: &reqwest::Client,
    mut reader: TempFile,
    mime_type: Mime,
    target_url: impl IntoUrl,
) -> anyhow::Result<()> {
    let target_url: reqwest::Url = target_url.into_url()?;
    let target_url_string = target_url.to_string();
    debug!("uploading file"; "url" => &target_url_string);

    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    let md5sum = hex::encode(Md5::digest(&bytes));

    // Content-Type must be part of the presigned URL's signed params
    // (ContentType=... when generating it).
    let req = client
        .put(target_url)
        .header(header::CONTENT_TYPE, mime_type.as_ref())
        .body(bytes)
        .build()?;
    let response = client.execute(req).await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("upload failed with status {status}: {body}");
    }
    let etag = response
        .headers()
        .get(ETAG)
        .ok_or(anyhow!("ETAG header not found"))?
        .to_str()?
        .to_owned();

    // strip leading and trailing "
    let etag = etag.strip_prefix('"').unwrap_or(&etag);
    let etag = etag.strip_suffix('"').unwrap_or(etag);

    trace!("uploaded file"; "url" => target_url_string, "md5sum" => &md5sum, "etag" => etag);

    ensure!(md5sum == etag, "ETAG not like md5sum!");
    Ok(())
}

#[cfg(test)]
mod test {
    use anyhow::Result;
    use async_tempfile::TempFile;
    use aws_sdk_s3::presigning::PresigningConfig;
    use mime_guess::mime;
    use std::{env, time::Duration};
    use tokio::io::AsyncWriteExt;

    async fn s3_config() -> aws_sdk_s3::Config {
        let endpoint_url = env::var("AWS_ENDPOINT_URL").unwrap();
        aws_sdk_s3::Config::new(
            &aws_config::load_from_env()
                .await
                .to_builder()
                .endpoint_url(endpoint_url)
                .build(),
        )
        .to_builder()
        .force_path_style(true)
        .build()
    }

    async fn remove_bucket_key(bucket: impl Into<String>, key: impl Into<String>) -> Result<()> {
        let client = aws_sdk_s3::Client::from_conf(s3_config().await);
        let _ = client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await?;
        Ok(())
    }

    async fn get_presigned_put_url(
        bucket: impl Into<String>,
        key: impl Into<String>,
        presigned_ttl: Duration,
    ) -> Result<String> {
        let client = aws_sdk_s3::Client::from_conf(s3_config().await);
        let presigning_config = PresigningConfig::builder()
            .expires_in(presigned_ttl)
            .build()?;

        let result = client
            .put_object()
            .bucket(bucket)
            .key(key)
            .presigned(presigning_config)
            .await?;
        Ok(result.uri().to_string())
    }

    // Requires a live S3-compatible endpoint (AWS_ENDPOINT_URL, S3_BUCKET, creds).
    // Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs live S3"]
    async fn test_presigned_put() -> Result<()> {
        let bucket = env::var("S3_BUCKET").unwrap();
        let key = "test-key";
        let presigned_ttl = Duration::from_secs(5);
        let reqwest_client = reqwest::Client::new();

        let tempfile = TempFile::new().await?;
        {
            let mut rw = tempfile.open_rw().await?;
            for _ in 0..250_000 {
                rw.write_all(b"Test data\n").await?;
            }
        }
        let presigned_url = get_presigned_put_url(&bucket, key, presigned_ttl).await?;
        let mime_type = mime::TEXT_PLAIN;
        super::upload_file(&reqwest_client, tempfile, mime_type, presigned_url).await?;

        // cleanup
        let _ = remove_bucket_key(&bucket, key).await;

        Ok(())
    }
}
