//! Screen thumbnails: a small JPEG of each Agent's main display.
//!
//! Agents upload straight to R2 through this Worker every few minutes, and the
//! dashboard reads them back the same way. Neither path touches a Durable
//! Object, so images never cross the coordinator's WebSocket or the company
//! presence stream. Only the latest image of each device is kept.
use crate::*;

/// The R2 bucket binding that stores the images.
const THUMBNAIL_BUCKET: &str = "THUMBNAILS";
/// Matches the Agent's limit. A 640 by 400 JPEG is normally 20–80 KiB.
pub(crate) const MAX_THUMBNAIL_BYTES: usize = 512 * 1024;
const JPEG_MAGIC: [u8; 3] = [0xff, 0xd8, 0xff];

/// Keys start with the company, so a dashboard user can only name images of
/// their own company, and a company's images share a prefix.
pub(crate) fn thumbnail_key(company_id: &str, device_id: &str) -> String {
    format!("thumbnails/{company_id}/{device_id}.jpg")
}

/// The ETag an `If-None-Match` header names, without quotes or a weak prefix,
/// which is how R2 compares it.
fn requested_etag(header: &str) -> Option<&str> {
    let etag = header.trim();
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    let etag = etag.trim_matches('"');
    (!etag.is_empty() && !etag.contains([',', '"', '*'])).then_some(etag)
}

/// Why `bytes` cannot be a thumbnail the Agent encoded, as a status and message.
fn thumbnail_problem(bytes: &[u8]) -> Option<(u16, &'static str)> {
    if bytes.len() > MAX_THUMBNAIL_BYTES {
        Some((413, "thumbnail is larger than 512 KiB"))
    } else if !bytes.starts_with(&JPEG_MAGIC) {
        Some((400, "thumbnail must be a JPEG"))
    } else {
        None
    }
}

/// `PUT /v1/agents/{device_id}/thumbnail`, called by the Agent with its own
/// credential. Replaces the device's image.
pub(crate) async fn upload_agent_thumbnail(
    request: &mut Request,
    environment: &Env,
    device_id: &str,
) -> Result<Response> {
    let authorization = match authorize_agent(request, environment, device_id).await {
        Ok(authorization) => authorization,
        Err(_) => return api_error(401, "Agent authentication failed"),
    };
    // A device being removed must not put back the image its deletion removed.
    if authorization.deletion_requested {
        return api_error(409, "the Agent is being removed");
    }
    let declared = request
        .headers()
        .get("Content-Length")?
        .and_then(|length| length.parse::<usize>().ok());
    if declared.is_some_and(|length| length > MAX_THUMBNAIL_BYTES) {
        return api_error(413, "thumbnail is larger than 512 KiB");
    }
    let bytes = request.bytes().await?;
    if let Some((status, message)) = thumbnail_problem(&bytes) {
        return api_error(status, message);
    }
    environment
        .bucket(THUMBNAIL_BUCKET)?
        .put(thumbnail_key(&authorization.company_id, device_id), bytes)
        .http_metadata(HttpMetadata {
            content_type: Some("image/jpeg".into()),
            ..Default::default()
        })
        .execute()
        .await?;
    Response::empty().map(|response| response.with_status(204))
}

/// `GET /v1/agents/{device_id}/thumbnail` for the dashboard. Answers a
/// matching `If-None-Match` with 304 so an unchanged image is not sent again,
/// and a device without an image with 204, which browsers do not log as an error.
pub(crate) async fn get_agent_thumbnail(
    request: &Request,
    environment: &Env,
    device_id: &str,
) -> Result<Response> {
    let identity = match authorize_workos_user(request, environment).await {
        Ok(identity) => identity,
        Err(error) => return workos_auth_error(error),
    };
    validate_identifier(device_id, "device ID")?;
    let if_none_match = request.headers().get("If-None-Match")?;
    let bucket = environment.bucket(THUMBNAIL_BUCKET)?;
    let mut get = bucket.get(thumbnail_key(&identity.company_id, device_id));
    if let Some(etag) = if_none_match.as_deref().and_then(requested_etag) {
        get = get.only_if(Conditional {
            etag_does_not_match: Some(etag.to_owned()),
            ..Default::default()
        });
    }
    let Some(object) = get.execute().await? else {
        return Response::empty().map(|response| response.with_status(204));
    };
    let headers = Headers::new();
    headers.set("ETag", &object.http_etag())?;
    headers.set(
        "Last-Modified",
        &String::from(js_sys::Date::from(object.uploaded()).to_utc_string()),
    )?;
    // The dashboard revalidates itself; nothing between may keep a copy.
    headers.set("Cache-Control", "private, no-cache")?;
    let Some(body) = object.body() else {
        return Ok(Response::empty()?.with_status(304).with_headers(headers));
    };
    headers.set("Content-Type", "image/jpeg")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    Ok(Response::from_bytes(body.bytes().await?)?.with_headers(headers))
}

/// Removes a deleted device's image.
pub(crate) async fn delete_agent_thumbnail(
    environment: &Env,
    company_id: &str,
    device_id: &str,
) -> Result<()> {
    environment
        .bucket(THUMBNAIL_BUCKET)?
        .delete(thumbnail_key(company_id, device_id))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_none_match_names_one_etag() {
        assert_eq!(requested_etag("\"abc123\""), Some("abc123"));
        assert_eq!(requested_etag("W/\"abc123\""), Some("abc123"));
        assert_eq!(requested_etag(" abc123 "), Some("abc123"));
        assert_eq!(requested_etag("*"), None);
        assert_eq!(requested_etag("\"a\", \"b\""), None);
        assert_eq!(requested_etag("\"\""), None);
    }

    #[test]
    fn thumbnails_must_be_small_jpegs() {
        assert_eq!(thumbnail_problem(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]), None);
        assert_eq!(
            thumbnail_problem(b"\x89PNG\r\n\x1a\n").map(|(status, _)| status),
            Some(400)
        );
        assert_eq!(thumbnail_problem(&[]).map(|(status, _)| status), Some(400));
        let mut large = vec![0; MAX_THUMBNAIL_BYTES + 1];
        large[..3].copy_from_slice(&JPEG_MAGIC);
        assert_eq!(
            thumbnail_problem(&large).map(|(status, _)| status),
            Some(413)
        );
        large.truncate(MAX_THUMBNAIL_BYTES);
        assert_eq!(thumbnail_problem(&large), None);
    }

    #[test]
    fn keys_are_scoped_to_the_company() {
        assert_eq!(
            thumbnail_key("company-acme", "device-1"),
            "thumbnails/company-acme/device-1.jpg"
        );
    }
}
