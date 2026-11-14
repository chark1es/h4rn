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
/// or cooling down. It is the head of [`eligible_failovers`].
pub fn pick_failover(
    current: &str,
    policy: &ProxyFailoverPolicy,
    models: &[ProxyModel],
    limits: &ProxyLimits,
) -> Option<ProxyFailoverChoice> {
    eligible_failovers(current, policy, models, limits)
        .into_iter()
        .next()
}

/// Every model (and account) a thread could fail over to, best first.
///
/// The order is the policy's: the head is what [`pick_failover`] returns and
/// what the daemon uses as the default, and the rest are alternatives a person
/// may choose instead. Models that are excluded or cooling down are left out.
/// When a healthy credential could still serve `current` (it has no cooldown
/// for it), "the same model on another account" is offered too: first under
/// [`ProxyFailoverStrategy::AccountFirst`], last otherwise.
pub fn eligible_failovers(
    current: &str,
    policy: &ProxyFailoverPolicy,
    models: &[ProxyModel],
    limits: &ProxyLimits,
) -> Vec<ProxyFailoverChoice> {
    let now = Utc::now();
    let cooled = cooled_models(limits, now);
    let candidates: Vec<&ProxyModel> = models
        .iter()
        .filter(|model| model.id != current)
        .filter(|model| !policy.exclude.iter().any(|excluded| excluded == &model.id))
        .filter(|model| !cooled.contains(&model.id))
        .collect();

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

    let mut ordered = candidates;
    match policy.strategy {
        ProxyFailoverStrategy::SameFamilyFirst => {
            ordered.sort_by_key(|model| family_of(&model.id) != family);
        }
        ProxyFailoverStrategy::AccountFirst => {
            ordered.sort_by_key(|model| healthy_account(&model.id).is_none());
        }
        ProxyFailoverStrategy::AnyModel => {}
    }

    let mut choices: Vec<ProxyFailoverChoice> = ordered
        .iter()
        .map(|pick| {
            let account = healthy_account(&pick.id);
            let mut reason = match policy.strategy {
                ProxyFailoverStrategy::SameFamilyFirst if family_of(&pick.id) == family => {
                    format!("same family as {current} and not cooling down")
                }
                ProxyFailoverStrategy::AccountFirst if account.is_some() => {
                    "a credential can serve this model without a cooldown".to_owned()
                }
                _ => format!("{current} was limited; this model has no active cooldown"),
            };
            if !limits.available {
                reason.push_str(" (credential cooldowns were unavailable)");
            }
            ProxyFailoverChoice {
                model: pick.id.clone(),
                account_label: account.as_deref().map(h4rn_protocol::credential_label),
                account,
                reason,
            }
        })
        .collect();

    // The same model on an account that is not cooling down for it. It needs
    // the cooldown snapshot: without one there is nothing to tell accounts
    // apart.
    let same_model_elsewhere = (limits.available && !cooled.iter().any(|model| model == current))
        .then(|| healthy_account(current))
        .flatten()
        .filter(|_| {
            models.iter().any(|model| model.id == current)
                && !policy.exclude.iter().any(|excluded| excluded == current)
                && limits.credentials.iter().filter(|c| !c.disabled).count() > 1
        })
        .map(|account| ProxyFailoverChoice {
            model: current.to_owned(),
            account_label: Some(h4rn_protocol::credential_label(&account)),
            reason: format!("{current} on another account that has no cooldown for it"),
            account: Some(account),
        });
    if let Some(choice) = same_model_elsewhere {
        if matches!(policy.strategy, ProxyFailoverStrategy::AccountFirst) {
            choices.insert(0, choice);
        } else {
            choices.push(choice);
        }
    }
    choices
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

    fn limits_with(credentials: Vec<ProxyCredential>) -> ProxyLimits {
        limits(credentials)
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
    fn every_eligible_choice_is_listed_best_first() {
        let models = vec![
            model("claude-sonnet-5", "anthropic"),
            model("gpt-6.1-sol", "openai"),
            model("claude-opus-5", "anthropic"),
            model("claude-haiku-5", "anthropic"),
        ];
        let limits = limits(vec![cooled("claude-haiku-5")]);
        let all = eligible_failovers(
            "claude-sonnet-5",
            &ProxyFailoverPolicy::default(),
            &models,
            &limits,
        );
        let ids: Vec<&str> = all.iter().map(|choice| choice.model.as_str()).collect();
        // Same family before other vendors; the cooled model never appears.
        assert_eq!(ids, ["claude-opus-5", "gpt-6.1-sol"]);
        assert_eq!(
            all.first(),
            pick_failover(
                "claude-sonnet-5",
                &ProxyFailoverPolicy::default(),
                &models,
                &limits
            )
            .as_ref()
        );
    }

    #[test]
    fn another_account_for_the_same_model_is_offered() {
        let models = vec![model("claude-sonnet-5", "anthropic")];
        let healthy = |name: &str| ProxyCredential {
            auth_index: Some(name.into()),
            name: Some(name.into()),
            provider: Some("claude".into()),
            status: None,
            disabled: false,
            success: None,
            failed: None,
            supports_quota: false,
            cooldowns: Vec::new(),
        };
        let limits = limits_with(vec![healthy("acct-a"), healthy("acct-b")]);
        let all = eligible_failovers(
            "claude-sonnet-5",
            &ProxyFailoverPolicy::default(),
            &models,
            &limits,
        );
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].model, "claude-sonnet-5");
        assert_eq!(all[0].account.as_deref(), Some("acct-a"));
        // One account is no alternative.
        let single = eligible_failovers(
            "claude-sonnet-5",
            &ProxyFailoverPolicy::default(),
            &models,
            &limits_with(vec![healthy("acct-a")]),
        );
        assert!(single.is_empty());
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
