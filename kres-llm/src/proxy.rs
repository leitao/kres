//! Proxy detection from `https_proxy` / `http_proxy` env vars.
//!
//! Matches `build_http_client` semantics:
//! prefer HTTPS-proxy vars over HTTP-proxy ones. Within each family
//! the lowercase form wins when set — matching the convention curl,
//! git, and most Python libraries follow. The returned value is a
//! trimmed URL string suitable for `reqwest::Proxy::all(..)`.

use std::env;

pub fn detect_proxy() -> Option<String> {
    // Priority: https_proxy (lower), HTTPS_PROXY, http_proxy (lower),
    // HTTP_PROXY. First non-empty wins. Value is trimmed so a
    // trailing newline in the env var doesn't make reqwest reject it.
    for var in &["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"] {
        if let Ok(v) = env::var(var) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global — serialize tests that mutate them.
    // (A regular `static` `Mutex` suffices for this small test module;
    // we avoid `once_cell`/`LazyLock` for stable-Rust minimalism.)
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static L: Mutex<()> = Mutex::new(());
        L.lock().unwrap()
    }

    fn clear_all() {
        for v in ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY"] {
            env::remove_var(v);
        }
    }

    #[test]
    fn none_when_unset() {
        let _g = lock();
        clear_all();
        assert_eq!(detect_proxy(), None);
    }

    #[test]
    fn picks_https_proxy_first() {
        let _g = lock();
        clear_all();
        env::set_var("http_proxy", "http://fallback:8080");
        env::set_var("https_proxy", "http://preferred:3128");
        assert_eq!(detect_proxy(), Some("http://preferred:3128".to_string()));
        clear_all();
    }

    #[test]
    fn ignores_blank_value() {
        let _g = lock();
        clear_all();
        env::set_var("https_proxy", "   ");
        env::set_var("http_proxy", "http://actual:8080");
        assert_eq!(detect_proxy(), Some("http://actual:8080".to_string()));
        clear_all();
    }

    #[test]
    fn falls_back_to_uppercase() {
        let _g = lock();
        clear_all();
        env::set_var("HTTPS_PROXY", "http://upper:3128");
        assert_eq!(detect_proxy(), Some("http://upper:3128".to_string()));
        clear_all();
    }
}
