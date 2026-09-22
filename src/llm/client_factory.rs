//! Helper to create LLM clients from configuration

use super::clients::LlmClient;

/// Create an LLM client from the config provider.
///
/// Refuses to build a client for empty or placeholder credentials (M5):
/// with the shipped config (`api_key = "${OPENAI_API_KEY}"`) and the env var
/// unset, `config::resolve` leaves the literal `${OPENAI_API_KEY}` behind —
/// without this guard the first run would make real provider calls with that
/// dummy key. Callers already treat `Err` as "provider unavailable" (the
/// agent slot / advisor is skipped), so refusing here follows the existing
/// disabled path with a single `tracing::warn!` per provider per game start.
pub fn create_llm_client(
    provider_type: &str,
    model: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> anyhow::Result<LlmClient> {
    match provider_type {
        "openai" => {
            let api_key =
                api_key.ok_or_else(|| anyhow::anyhow!("OpenAI client requires api_key"))?;
            reject_placeholder_key(provider_type, api_key)?;
            let base_url = base_url.unwrap_or("https://api.openai.com");
            Ok(LlmClient::new_openai(
                api_key.to_string(),
                model.to_string(),
                base_url.to_string(),
            ))
        }
        "ollama" => {
            let base_url = base_url.unwrap_or("http://localhost:11434");
            Ok(LlmClient::new_ollama(
                base_url.to_string(),
                model.to_string(),
            ))
        }
        "anthropic" => {
            let api_key =
                api_key.ok_or_else(|| anyhow::anyhow!("Anthropic client requires api_key"))?;
            reject_placeholder_key(provider_type, api_key)?;
            let base_url = base_url.unwrap_or("https://api.anthropic.com");
            Ok(LlmClient::new_anthropic(
                api_key.to_string(),
                model.to_string(),
                base_url.to_string(),
            ))
        }
        _ => anyhow::bail!("Unknown provider type: {}", provider_type),
    }
}

/// Err if `key` is empty or a known placeholder, with the single
/// "LLM disabled: placeholder credentials for <provider>" warning.
fn reject_placeholder_key(provider_type: &str, key: &str) -> anyhow::Result<()> {
    if is_placeholder_api_key(key) {
        tracing::warn!("LLM disabled: placeholder credentials for {provider_type}");
        anyhow::bail!(
            "placeholder or empty api_key for {provider_type} — refusing to call the provider with dummy credentials (set a real key, or the env var it expands from)"
        );
    }
    Ok(())
}

/// True when `key` is empty or a known placeholder: an unexpanded `${ENV_VAR}`
/// reference (what `config::resolve` leaves behind when the variable is
/// unset), an empty/whitespace value, a documented example key, or an
/// `sk-`-style key whose tail is an obvious example token. Real keys are
/// long random strings, so none of these patterns collide in practice; a
/// false positive only disables the LLM with a loud, fixable warning.
pub fn is_placeholder_api_key(key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return true;
    }
    // Unexpanded env reference, e.g. "${OPENAI_API_KEY}".
    if key.starts_with("${") && key.ends_with('}') && key.len() >= 4 {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    // Whole-value placeholders.
    const PLACEHOLDERS: &[&str] = &[
        "example",
        "example-key",
        "placeholder",
        "your-api-key",
        "your_api_key",
        "yourkey",
        "changeme",
        "change-me",
        "change_me",
        "dummy",
        "fake",
        "fake-key",
        "not-a-real-key",
        "redacted",
        "todo",
        "sk-xxx",
        "sk-xxxx",
        "sk-xxxxx",
    ];
    if PLACEHOLDERS.iter().any(|p| lower == *p) {
        return true;
    }
    // "sk-"-prefixed keys (OpenAI/Anthropic style) beginning with an obvious
    // example token.
    if let Some(rest) = lower.strip_prefix("sk-") {
        const SK_PREFIXES: &[&str] = &[
            "your",
            "fake",
            "dummy",
            "example",
            "placeholder",
            "test",
            "xxx",
            "abc123",
        ];
        if SK_PREFIXES.iter().any(|p| rest.starts_with(p)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_detection_empty_and_whitespace() {
        assert!(is_placeholder_api_key(""));
        assert!(is_placeholder_api_key("   "));
    }

    #[test]
    fn placeholder_detection_unexpanded_env_reference() {
        // The shipped config.toml shape: env var unset → literal stays.
        assert!(is_placeholder_api_key("${OPENAI_API_KEY}"));
        assert!(is_placeholder_api_key("${ANTHROPIC_API_KEY}"));
        // A real key never contains `${`/`}` pairs — but the pattern is the
        // whole value, so anything with extra text is not a placeholder.
        assert!(!is_placeholder_api_key("sk-real-key${tail}more"));
    }

    #[test]
    fn placeholder_detection_known_examples() {
        assert!(is_placeholder_api_key("example"));
        assert!(is_placeholder_api_key("your-api-key"));
        assert!(is_placeholder_api_key("changeMe"));
        assert!(is_placeholder_api_key("fake"));
        assert!(is_placeholder_api_key("sk-xxxx"));
        assert!(is_placeholder_api_key("sk-your-api-key"));
        assert!(is_placeholder_api_key("sk-test"));
        assert!(is_placeholder_api_key("sk-abc123def456"));
    }

    #[test]
    fn placeholder_detection_real_looking_keys() {
        assert!(!is_placeholder_api_key(
            "sk-ant-api03-AbC123def456GhI789jKl012"
        ));
        assert!(!is_placeholder_api_key("sk-proj-XYz987AbCdeF012"));
        assert!(!is_placeholder_api_key("a1b2c3d4e5f6g7h8i9j0"));
    }

    #[test]
    fn factory_rejects_placeholder_keys_without_creating_client() {
        for provider in ["openai", "anthropic"] {
            let err = create_llm_client(provider, "model", Some("${OPENAI_API_KEY}"), None)
                .err()
                .unwrap();
            assert!(err.to_string().contains("placeholder"), "{provider}: {err}");
        }
        // A real-looking key still builds.
        assert!(create_llm_client(
            "openai",
            "gpt-4o",
            Some("sk-proj-real-looking-0123456789"),
            None
        )
        .is_ok());
        // Ollama is local and needs no key: never disabled by the guard.
        assert!(create_llm_client("ollama", "llama3", None, None).is_ok());
        assert!(create_llm_client("ollama", "llama3", Some("${OLLAMA_KEY}"), None).is_ok());
    }
}
