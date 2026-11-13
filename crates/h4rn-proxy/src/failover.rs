//! Choosing the next model (and account) after a limit.
//!
//! CLIProxyAPI retries across credentials itself, but a client that received
//! the terminal error still has to decide what to send next. The helper here is
//! deliberately pure: it takes the model catalogue and the credential health
//! snapshot and returns a candidate, so Phase 5's auto-resume can call it
//! without another network round trip.

use chrono::Utc;
use h4rn_protocol::{
    ProxyFailoverChoice, ProxyFailoverPolicy, ProxyFailoverStrategy, ProxyLimits, ProxyModel,
};

/// Picks a model the proxy can serve that is not the one that just failed.
///
/// Returns `None` when there is no other model, or every candidate is excluded
/// or cooling down.
pub fn pick_failover(
    current: &str,
    policy: &ProxyFailoverPolicy,
    models: &[ProxyModel],
    limits: &ProxyLimits,
) -> Option<ProxyFailoverChoice> {
    let now = Utc::now();
    let cooled = cooled_models(limits, now);
    let candidates: Vec<&ProxyModel> = models
        .iter()
        .filter(|model| model.id != current)
        .filter(|model| !policy.exclude.iter().any(|excluded| excluded == &model.id))
        .filter(|model| !cooled.contains(&model.id))
        .collect();
    if candidates.is_empty() {
        return None;
    }

    let family = family_of(current);
    let healthy_account = |model: &str| -> Option<String> {
        limits
            .credentials
            .iter()
            .filter(|credential| !credential.disabled)
            .filter(|credential| {
                !credential.cooldowns.iter().any(|cooldown| {
                    active(cooldown, now)
                        && cooldown.model_key.as_deref().is_none_or(|key| key == model)
                })
            })
            .find_map(|credential| {
                credential
                    .name
                    .clone()
                    .or_else(|| credential.auth_index.clone())
            })
    };

    let pick = match policy.strategy {
        ProxyFailoverStrategy::SameFamilyFirst => candidates
            .iter()
            .find(|model| family_of(&model.id) == family)
            .copied()
            .or_else(|| candidates.first().copied()),
        ProxyFailoverStrategy::AccountFirst => candidates
            .iter()
            .find(|model| healthy_account(&model.id).is_some())
            .copied()
            .or_else(|| candidates.first().copied()),
        ProxyFailoverStrategy::AnyModel => candidates.first().copied(),
    }?;

    let account = healthy_account(&pick.id);
    let mut reason = match policy.strategy {
        ProxyFailoverStrategy::SameFamilyFirst if family_of(&pick.id) == family => {
            format!("same family as `{current}` and not cooling down")
        }
        ProxyFailoverStrategy::AccountFirst if account.is_some() => {
            "a credential can serve this model without a cooldown".to_owned()
        }
        _ => format!("`{current}` was limited; this model has no active cooldown"),
    };
    if !limits.available {
        reason.push_str(" (credential cooldowns were unavailable)");
    }
    Some(ProxyFailoverChoice {
        model: pick.id.clone(),
        account,
        reason,
    })
}

fn cooled_models(limits: &ProxyLimits, now: chrono::DateTime<Utc>) -> Vec<String> {
    limits
        .credentials
        .iter()
        .flat_map(|credential| credential.cooldowns.iter())
        .filter(|cooldown| active(cooldown, now))
        .filter_map(|cooldown| cooldown.model_key.clone())
        .collect()
}

fn active(cooldown: &h4rn_protocol::ProxyCooldown, now: chrono::DateTime<Utc>) -> bool {
    cooldown.retry_at.is_some_and(|retry_at| retry_at > now)
        || cooldown
            .remaining_seconds
            .is_some_and(|seconds| seconds > 0)
}

fn family_of(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    if let Some((head, _)) = lower.split_once('-')
        && !head.is_empty()
    {
        return head.to_owned();
    }
    if let Some((head, _)) = lower.split_once('.') {
        return head.to_owned();
    }
    lower
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::{
        ProxyCooldown, ProxyCredential, ProxyLimits, ProxyModelShape, ProxyModelSource,
        ProxyProfileId,
    };

    fn model(id: &str, owned_by: &str) -> ProxyModel {
        ProxyModel {
            id: id.into(),
            display_name: None,
            shape: ProxyModelShape::OpenAi,
            source: ProxyModelSource::OpenAi,
            alias_of: None,
            owned_by: Some(owned_by.into()),
            context_length: None,
            max_output_tokens: None,
            fork_enriched: false,
        }
    }

    fn limits(credentials: Vec<ProxyCredential>) -> ProxyLimits {
        ProxyLimits {
            profile_id: ProxyProfileId::new("proxy_0"),
            available: true,
            credentials,
            observed_at: Utc::now(),
            note: None,
        }
    }

    fn cooled(model_key: &str) -> ProxyCredential {
        ProxyCredential {
            auth_index: Some("1".into()),
            name: Some("acct-a".into()),
            provider: Some("claude".into()),
            status: Some("ok".into()),
            disabled: false,
            success: None,
            failed: None,
            supports_quota: false,
            cooldowns: vec![ProxyCooldown {
                scope: Some("model".into()),
                model_key: Some(model_key.into()),
                reason: Some("429".into()),
                retry_at: None,
                remaining_seconds: Some(60),
                backoff_level: Some(1),
                http_status: Some(429),
            }],
        }
    }

    #[test]
    fn same_family_is_preferred_and_cooled_models_are_skipped() {
        let models = vec![
            model("claude-sonnet-5", "anthropic"),
            model("claude-opus-5", "anthropic"),
            model("gpt-6.1-sol", "openai"),
        ];
        let limits = limits(vec![cooled("claude-sonnet-5")]);
        let choice = pick_failover(
            "claude-sonnet-5",
            &ProxyFailoverPolicy::default(),
            &models,
            &limits,
        )
        .expect("choice");
        assert_eq!(choice.model, "claude-opus-5");
        assert!(choice.reason.contains("same family"));
    }

    #[test]
    fn any_model_falls_back_when_the_family_is_exhausted() {
        let models = vec![
            model("claude-sonnet-5", "anthropic"),
            model("gpt-6.1-sol", "openai"),
        ];
        let policy = ProxyFailoverPolicy {
            strategy: ProxyFailoverStrategy::AnyModel,
            max_backoff_level: None,
            exclude: Vec::new(),
        };
        let choice =
            pick_failover("claude-sonnet-5", &policy, &models, &limits(vec![])).expect("choice");
        assert_eq!(choice.model, "gpt-6.1-sol");
    }

    #[test]
    fn exclusions_and_all_cooled_return_none() {
        let models = vec![model("a-one", "a"), model("b-two", "b")];
        let policy = ProxyFailoverPolicy {
            exclude: vec!["b-two".into()],
            ..Default::default()
        };
        let limits = limits(vec![cooled("a-one"), cooled("b-two")]);
        assert!(pick_failover("a-one", &policy, &models, &limits).is_none());
    }

    #[test]
    fn account_first_reports_a_healthy_credential() {
        let models = vec![model("gpt-6.1-sol", "openai")];
        let limits = limits(vec![ProxyCredential {
            auth_index: Some("2".into()),
            name: Some("acct-b".into()),
            provider: Some("openai".into()),
            status: None,
            disabled: false,
            success: None,
            failed: None,
            supports_quota: false,
            cooldowns: Vec::new(),
        }]);
        let policy = ProxyFailoverPolicy {
            strategy: ProxyFailoverStrategy::AccountFirst,
            max_backoff_level: None,
            exclude: Vec::new(),
        };
        let choice = pick_failover("claude-sonnet-5", &policy, &models, &limits).expect("choice");
        assert_eq!(choice.model, "gpt-6.1-sol");
        assert_eq!(choice.account.as_deref(), Some("acct-b"));
    }
}
