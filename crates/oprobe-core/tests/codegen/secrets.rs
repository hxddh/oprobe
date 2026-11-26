use opendal::layers::{RetryLayer, TimeoutLayer};
use opendal::Operator;

/// The operator as oprobe measured it.
pub fn build_operator() -> Result<Operator, Box<dyn std::error::Error + Send + Sync>> {
    let op = Operator::via_iter(
        "s3",
        [
            ("access_key_id".to_owned(), std::env::var("AWS_ACCESS_KEY_ID")?),
            ("bucket".to_owned(), "bucket".to_owned()),
            ("endpoint".to_owned(), format!("https://{}:9000/{{tenant}}", std::env::var("S3_HOST")?)),
            ("region".to_owned(), "eu-west-1".to_owned()),
            ("root".to_owned(), "/prefix".to_owned()),
            // secret_access_key: a secret, not written here: your credential chain provides it
            // session_token: a secret, not written here: your credential chain provides it
        ],
    )?
    // Innermost first: each layer wraps the ones before it.
    .layer(TimeoutLayer::new())
    .layer(RetryLayer::new());
    Ok(op)
}
