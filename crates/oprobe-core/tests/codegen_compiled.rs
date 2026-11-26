//! The compile check of `to_rust()`: every `tests/codegen/NAME.rs` (exactly what `to_rust()`
//! generates for `NAME.toml`, kept so by `codegen.rs`) is a module of this test, so building
//! the tests compiles the generated code against the workspace's OpenDAL, foyer and reqwest.
//! The operators that need no network are built and used as well.

use opendal::Operator;

macro_rules! fixtures {
    ($($name:ident),* $(,)?) => {
        $(mod $name {
            include!(concat!("codegen/", stringify!($name), ".rs"));
        })*

        /// Every generated `build_operator` (named, so none is dead code).
        fn builders() -> usize {
            let mut count = 0;
            $(
                let _ = $name::build_operator;
                count += 1;
            )*
            count
        }
    };
}

fixtures! {
    cache,
    concurrent_limit_http,
    concurrent_limit_operations,
    defaults,
    every_layer,
    fs_relative,
    http_every_option,
    http_h1,
    http_h2_alpn,
    http_h2_prior_knowledge,
    http_no_proxy,
    lab,
    no_layers,
    options_alias,
    read_options,
    retry,
    secrets,
    tail_cut,
    tail_cut_opendal_defaults,
    tail_cut_under_timeout,
    throttle,
    timeout,
    webdav,
    write_options,
}

/// Write an object through `op` and read it back.
async fn round_trip(op: &Operator) {
    let data = vec![7u8; 64 * 1024];
    op.write("dir/object", data.clone()).await.unwrap();
    assert_eq!(op.read("dir/object").await.unwrap().to_vec(), data);
    assert_eq!(
        op.stat("dir/object").await.unwrap().content_length(),
        64 * 1024
    );
}

#[test]
fn every_generated_operator_compiles() {
    assert_eq!(builders(), 24);
    // The reader of the operator that sets an HTTP client too (not built: it needs S3).
    let _ = http_every_option::reader;
}

#[tokio::test]
async fn the_generated_layers_build_and_serve() {
    for op in [
        no_layers::build_operator(),
        timeout::build_operator(),
        tail_cut::build_operator(),
        tail_cut_under_timeout::build_operator(),
        tail_cut_opendal_defaults::build_operator(),
        concurrent_limit_http::build_operator(),
        concurrent_limit_operations::build_operator(),
        throttle::build_operator(),
        retry::build_operator(),
    ] {
        round_trip(&op.unwrap()).await;
    }
}

#[tokio::test]
async fn the_generated_cache_builds_and_takes_an_object_of_its_budget() {
    let op = cache::build_operator().await.unwrap();
    round_trip(&op).await;
    // `cache = { memory = "1MiB" }`: an object of 1 MiB is within the budget.
    let data = vec![1u8; 1024 * 1024];
    op.write("big", data.clone()).await.unwrap();
    assert_eq!(op.read("big").await.unwrap().to_vec(), data);
    assert_eq!(op.read_with("big").range(10..20).await.unwrap().len(), 10);
}

#[tokio::test]
async fn the_generated_reader_and_writer_apply_their_options() {
    let op = every_layer::build_operator().await.unwrap();
    let data = vec![3u8; 256 * 1024];
    let mut w = every_layer::writer(&op, "a").await.unwrap();
    w.write(data.clone()).await.unwrap();
    w.close().await.unwrap();
    let r = every_layer::reader(&op, "a").await.unwrap();
    assert_eq!(r.read(..).await.unwrap().to_vec(), data);

    let op = write_options::build_operator().unwrap();
    let mut w = write_options::writer(&op, "b").await.unwrap();
    w.write(data.clone()).await.unwrap();
    w.close().await.unwrap();
    let op = read_options::build_operator().unwrap();
    op.write("c", data.clone()).await.unwrap();
    let r = read_options::reader(&op, "c").await.unwrap();
    assert_eq!(r.read(0..1024).await.unwrap().len(), 1024);
}
