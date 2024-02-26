use std::sync::{Arc, Mutex};

use anyhow::{anyhow, ensure};
use async_tempfile::TempFile;
use foundations::telemetry::log::{debug, trace};
use md5::{Digest, Md5};
use reqwest::{header::ETAG, IntoUrl};
use tokio_util::io::{InspectReader, ReaderStream};

pub async fn upload_file(
    client: &reqwest::Client,
    reader: TempFile,
    target_url: impl IntoUrl,
) -> anyhow::Result<()> {
    let target_url: reqwest::Url = target_url.into_url()?;
    let target_url_string = target_url.to_string();
    debug!("uploading file"; "url" => &target_url_string);

    let hasher = Md5::new();
    let hasher_rc = Arc::new(Mutex::new(hasher));
    let etag = {
        let hasher_rc2 = hasher_rc.clone();
        let hashing_reader = InspectReader::new(reader, move |bytes| {
            hasher_rc2.lock().unwrap().update(bytes)
        });
        let stream = ReaderStream::new(hashing_reader);
        let body = reqwest::Body::wrap_stream(stream);
        let req = client.put(target_url).body(body).build()?;
        client
            .execute(req)
            .await?
            .headers()
            .get(ETAG)
            .ok_or(anyhow!("ETAG header not found"))?
            .to_str()?
    }
    .to_owned();

    // strip leading and trailing "
    let etag = etag.strip_prefix('"').unwrap_or(&etag);
    let etag = etag.strip_suffix('"').unwrap_or(etag);

    let md5sum = Arc::try_unwrap(hasher_rc)
        .map_err(|_| anyhow!("Lock still has multiple owners!."))?
        .into_inner()?
        .finalize();
    let md5sum = format!("{:x}", md5sum);

    trace!("uploaded file"; "url" => target_url_string, "md5sum" => &md5sum, "etag" => etag);

    ensure!(md5sum == etag, "ETAG not like md5sum!");
    Ok(())
}

#[cfg(test)]
mod test {
    use anyhow::Result;
    use async_tempfile::TempFile;
    use aws_sdk_s3::presigning::PresigningConfig;
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

    #[tokio::test]
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
        let _ = super::upload_file(reqwest_client, tempfile, presigned_url).await?;

        // cleanup
        let _ = remove_bucket_key(&bucket, key).await;

        Ok(())
    }
}
