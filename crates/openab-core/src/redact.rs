//! Rendering session-bearing identifiers in logs without handing out the session.
//!
//! An ACP session is addressed two ways and both carry the same uuid: the session id is
//! `sess_<uuid>` and the channel id is `acp_<uuid>`. Either one is enough to resume — `sess_` is
//! taken directly by resume, and `acp_` differs from it only by prefix — so both are credentials,
//! and a redaction that covers one of them covers nothing.
//!
//! Ids also travel embedded: a pool key is `<platform>:<channel_id>`, so scanning for a field
//! named `channel` misses it entirely. Redaction here matches on the VALUE's shape, which is why
//! it can be applied to a composite without the caller taking it apart.
//!
//! LINE and Telegram identifiers are private messaging identities and are pseudonymized whenever
//! safe observability is explicitly enabled. Other non-ACP identifiers remain unchanged
//! deliberately: a Discord or Slack channel id is public and operators grep for it.

const SAFE_OBSERVABILITY_ENV: &str = "OPENAB_SAFE_OBSERVABILITY";

/// Whether the opt-in bounded telemetry and private-platform log policy is enabled.
pub fn safe_observability_enabled() -> bool {
    std::env::var(SAFE_OBSERVABILITY_ENV)
        .map(|value| matches!(value.as_str(), "1" | "true"))
        .unwrap_or(false)
}

/// Hash any `acp_`/`sess_` segment and any LINE/Telegram composite identity in `s`.
///
/// Segments are split on `:` so a `<platform>:<channel_id>` pool key redacts its id half and keeps
/// the platform readable.
///
/// The same uuid tags identically whichever prefix carried it, so one session reads as one tag
/// across every log line that mentions it — that correlation is the only reason to keep an
/// identifier in a log at all.
pub fn redact_session_ids(s: &str) -> String {
    redact_session_ids_with_mode(s, safe_observability_enabled())
}

/// Redact every session/platform identifier only after safe observability was
/// enabled. Use this for newly protected log sites so the disabled default
/// preserves their legacy output.
pub fn redact_session_ids_if_safe(s: &str) -> String {
    if safe_observability_enabled() {
        redact_session_ids_with_mode(s, true)
    } else {
        s.to_string()
    }
}

fn redact_session_ids_with_mode(s: &str, safe_observability: bool) -> String {
    if let Some((platform, identities)) = s.split_once(':') {
        if safe_observability && is_private_messaging_platform(platform) {
            return std::iter::once(platform.to_string())
                .chain(identities.split(':').map(hash_tag))
                .collect::<Vec<_>>()
                .join(":");
        }
    }

    s.split(':')
        .map(|seg| match seg.strip_prefix("acp_").or_else(|| seg.strip_prefix("sess_")) {
            Some(uuid) if !uuid.is_empty() => hash_tag(uuid),
            _ => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// Render a platform identity for logs. With safe observability enabled, LINE
/// and Telegram identifiers are reduced to a stable pseudonymous tag. Other
/// platforms preserve today's operator-facing ids, while ACP resume
/// credentials remain covered by [`redact_session_ids`] in either mode.
pub fn redact_platform_identity(platform: &str, id: &str) -> String {
    redact_platform_identity_with_mode(platform, id, safe_observability_enabled())
}

/// Platform-aware counterpart to [`redact_session_ids_if_safe`].
pub fn redact_platform_identity_if_safe(platform: &str, id: &str) -> String {
    if safe_observability_enabled() {
        redact_platform_identity_with_mode(platform, id, true)
    } else {
        id.to_string()
    }
}

pub(crate) fn redact_platform_identity_with_mode(
    platform: &str,
    id: &str,
    safe_observability: bool,
) -> String {
    if safe_observability && is_private_messaging_platform(platform) {
        hash_tag(id)
    } else {
        redact_session_ids_with_mode(id, safe_observability)
    }
}

fn is_private_messaging_platform(platform: &str) -> bool {
    matches!(platform, "line" | "telegram")
}

fn hash_tag(uuid: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(uuid.as_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("#{short}")
}

#[cfg(test)]
mod tests {
    use super::{
        redact_platform_identity_with_mode, redact_session_ids, redact_session_ids_with_mode,
    };

    /// A table, not a single vector — the branch structure is what needs pinning.
    ///
    /// One example only proves the hash. The predicate is the part most likely to move: this
    /// function exists because a redaction covering `acp_` but not `sess_` was shipped, and that
    /// edit changes which inputs hash without changing the output for any `acp_` input. A single
    /// `acp_...` vector cannot see it.
    #[test]
    fn both_encodings_hash_alike_and_everything_else_passes_through() {
        let u = "00000000-0000-0000-0000-000000000000";
        let tag = redact_session_ids(&format!("acp_{u}"));
        assert!(tag.starts_with('#') && tag.len() == 9, "expected #<8hex>, got {tag}");
        assert_eq!(
            redact_session_ids(&format!("sess_{u}")),
            tag,
            "both encodings carry the SAME uuid, so they must produce the same tag or one session \
             reads as two"
        );
        assert_eq!(
            redact_session_ids(&format!("acp:acp_{u}")),
            format!("acp:{tag}"),
            "a <platform>:<id> pool key must redact the id half and keep the platform greppable"
        );
        assert_eq!(redact_session_ids("1234567890"), "1234567890", "public ids stay greppable");
        assert_eq!(redact_session_ids("-"), "-", "the no-session sentinel is not a session");
        assert_eq!(redact_session_ids(""), "", "empty in, empty out");
        assert_eq!(redact_session_ids("acp_"), "acp_", "a bare prefix carries no uuid to hide");
        assert_eq!(
            redact_session_ids("discord:1234567890"),
            "discord:1234567890",
            "a non-ACP composite is untouched, which is why applying this blindly is safe"
        );
    }

    #[test]
    fn private_messaging_ids_and_composite_keys_are_pseudonymized() {
        let telegram = redact_platform_identity_with_mode("telegram", "1234567890", true);
        assert!(telegram.starts_with('#') && telegram.len() == 9);
        assert!(!telegram.contains("1234567890"));
        assert_eq!(
            telegram,
            redact_platform_identity_with_mode("telegram", "1234567890", true)
        );

        let line = redact_session_ids_with_mode("line:U1234567890abcdef", true);
        assert!(line.starts_with("line:#"));
        assert!(!line.contains("U1234567890abcdef"));

        let telegram_composite =
            redact_session_ids_with_mode("telegram:1234567890:9876543210", true);
        assert_eq!(telegram_composite.matches('#').count(), 2);
        assert!(!telegram_composite.contains("1234567890"));
        assert!(!telegram_composite.contains("9876543210"));

        assert_eq!(
            redact_platform_identity_with_mode("discord", "1234567890", true),
            "1234567890",
            "public platform ids keep their existing operator semantics"
        );
    }

    #[test]
    fn private_messaging_redaction_is_explicitly_opt_in() {
        assert_eq!(
            redact_platform_identity_with_mode("telegram", "1234567890", false),
            "1234567890"
        );
        assert_eq!(
            redact_session_ids_with_mode("line:U1234567890abcdef", false),
            "line:U1234567890abcdef"
        );
    }
}
