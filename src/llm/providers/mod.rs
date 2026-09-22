pub mod anthropic;
pub mod ollama;
pub mod openai;

/// Default total request timeout for LLM HTTP calls. A hung endpoint must not
/// park an LLM player for the rest of the game.
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Default TCP connect timeout for LLM HTTP calls.
pub const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Build a shared (connection-pooled) `reqwest::Client` with a total request
/// timeout and a [`DEFAULT_CONNECT_TIMEOUT`] connect timeout.
///
/// Pass `None` to use the [`DEFAULT_TIMEOUT`]. The builder can only fail on
/// zero durations or platform socket init, which we treat as invariants.
pub fn build_http_client(timeout: Option<std::time::Duration>) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout.unwrap_or(DEFAULT_TIMEOUT))
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .build()
        .expect("reqwest client must build with non-zero timeouts")
}
