//! Removes `Content-Encoding` from captured bodies for display, and from
//! bodies handed to scripts.

use async_compression::tokio::bufread::{
    BrotliDecoder, DeflateDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder,
};
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Decodes `raw` according to a `Content-Encoding` value, keeping at most
/// `limit` bytes of output. Returns `None` for identity or unknown encodings
/// and for input that decodes to nothing. A body that was truncated at capture
/// time decodes as far as its bytes go.
pub(crate) async fn decode(encoding: &str, raw: &[u8], limit: usize) -> Option<Bytes> {
    let codings = codings(encoding);
    if codings.is_empty() {
        return None;
    }
    decode_codings(&codings, raw, limit, false).await
}

/// Decodes all of `raw`, for a body that is rewritten and forwarded rather
/// than displayed. Identity returns `raw` itself. Returns `None` when `raw`
/// does not decode completely to at most `limit` bytes: an unknown coding,
/// corrupt or cut-off input, or more output than `limit`.
pub(crate) async fn decode_all(encoding: &str, raw: &Bytes, limit: usize) -> Option<Bytes> {
    let codings = codings(encoding);
    if codings.is_empty() || raw.is_empty() {
        return (raw.len() <= limit).then(|| raw.clone());
    }
    decode_codings(&codings, raw, limit, true).await
}

fn codings(encoding: &str) -> Vec<String> {
    encoding
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| !c.is_empty() && c != "identity")
        .collect()
}

async fn decode_codings(
    codings: &[String],
    raw: &[u8],
    limit: usize,
    strict: bool,
) -> Option<Bytes> {
    // Codings are listed in the order they were applied, so undo them in reverse.
    let mut data = Bytes::copy_from_slice(raw);
    for coding in codings.iter().rev() {
        data = decode_one(coding, &data, limit, strict).await?;
    }
    Some(data)
}

async fn decode_one(coding: &str, input: &[u8], limit: usize, strict: bool) -> Option<Bytes> {
    let decoded = match coding {
        "gzip" | "x-gzip" => {
            let mut decoder = GzipDecoder::new(input);
            decoder.multiple_members(true);
            read_capped(decoder, limit, strict).await
        }
        "br" => read_capped(BrotliDecoder::new(input), limit, strict).await,
        "zstd" => read_capped(ZstdDecoder::new(input), limit, strict).await,
        // `deflate` is meant to be zlib-wrapped, but some servers send raw deflate.
        "deflate" => match read_capped(ZlibDecoder::new(input), limit, strict).await {
            Some(out) => Some(out),
            None => read_capped(DeflateDecoder::new(input), limit, strict).await,
        },
        _ => None,
    }?;
    Some(decoded.into())
}

/// Reads up to `limit` bytes. With `strict`, anything short of the whole
/// output within `limit` is `None`.
async fn read_capped(
    decoder: impl AsyncRead + Unpin,
    limit: usize,
    strict: bool,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    // One byte past the limit tells a full output from a cut one.
    let cap = limit as u64 + u64::from(strict);
    match decoder.take(cap).read_to_end(&mut out).await {
        Ok(_) if strict && out.len() > limit => None,
        Ok(_) => Some(out),
        // Keep what decoded before the error, e.g. a body cut off at max_body_bytes.
        Err(_) if !strict && !out.is_empty() => Some(out),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZstdEncoder};
    use tokio::io::AsyncWriteExt;

    use super::*;

    const TEXT: &[u8] = b"hello hello hello hello hello hello hello hello";

    async fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = GzipEncoder::new(Vec::new());
        e.write_all(data).await.unwrap();
        e.shutdown().await.unwrap();
        e.into_inner()
    }

    #[tokio::test]
    async fn decodes_each_coding() {
        let mut br = BrotliEncoder::new(Vec::new());
        br.write_all(TEXT).await.unwrap();
        br.shutdown().await.unwrap();
        let mut zstd = ZstdEncoder::new(Vec::new());
        zstd.write_all(TEXT).await.unwrap();
        zstd.shutdown().await.unwrap();

        for (coding, body) in [
            ("gzip", gzip(TEXT).await),
            ("br", br.into_inner()),
            ("zstd", zstd.into_inner()),
        ] {
            assert_eq!(decode(coding, &body, 1024).await.unwrap(), TEXT, "{coding}");
        }
    }

    #[tokio::test]
    async fn undoes_stacked_codings_in_reverse() {
        let twice = gzip(&gzip(TEXT).await).await;
        assert_eq!(decode("gzip, gzip", &twice, 1024).await.unwrap(), TEXT);
    }

    #[tokio::test]
    async fn identity_and_unknown_are_not_decoded() {
        assert!(decode("identity", TEXT, 1024).await.is_none());
        assert!(decode("compress", TEXT, 1024).await.is_none());
    }

    #[tokio::test]
    async fn output_is_capped_and_truncated_input_decodes_partially() {
        let body = gzip(TEXT).await;
        assert_eq!(decode("gzip", &body, 5).await.unwrap(), &TEXT[..5]);
        let partial = decode("gzip", &body[..body.len() - 8], 1024).await.unwrap();
        assert!(TEXT.starts_with(&partial));
    }

    #[tokio::test]
    async fn decode_all_is_all_or_nothing() {
        let body = Bytes::from(gzip(TEXT).await);
        let limit = TEXT.len();
        assert_eq!(decode_all("gzip", &body, limit).await.unwrap(), TEXT);
        assert!(decode_all("gzip", &body, limit - 1).await.is_none());
        let cut = body.slice(..body.len() - 8);
        assert!(decode_all("gzip", &cut, 1024).await.is_none());
        assert!(decode_all("compress", &body, 1024).await.is_none());
        let plain = Bytes::from_static(TEXT);
        assert_eq!(decode_all("identity", &plain, limit).await.unwrap(), TEXT);
        assert!(decode_all("", &plain, limit - 1).await.is_none());
        assert!(
            decode_all("gzip", &Bytes::new(), 0)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
