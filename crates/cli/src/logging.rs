//! tracing setup.

use tracing_subscriber::EnvFilter;

pub fn init(level: &str, format: &str) {
    let filter = EnvFilter::try_new(level)
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("valid default filter")
        // onnxruntime warns about things like missing GPUs; only show errors.
        .add_directive("ort=error".parse().expect("valid directive"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
    if format == "json" {
        builder.json().init();
    } else {
        builder.with_target(false).init();
    }
}
