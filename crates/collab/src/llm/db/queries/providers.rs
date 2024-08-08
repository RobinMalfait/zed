use super::*;
use sea_orm::QueryOrder;
use std::str::FromStr;
use strum::IntoEnumIterator as _;

pub struct ModelRateLimits {
    pub max_requests_per_minute: i32,
    pub max_tokens_per_minute: i32,
    pub max_tokens_per_day: i32,
}

impl LlmDatabase {
    pub async fn initialize_providers(&mut self) -> Result<()> {
        self.provider_ids = self
            .transaction(|tx| async move {
                let existing_providers = provider::Entity::find().all(&*tx).await?;

                let mut new_providers = LanguageModelProvider::iter()
                    .filter(|provider| {
                        !existing_providers
                            .iter()
                            .any(|p| p.name == provider.to_string())
                    })
                    .map(|provider| provider::ActiveModel {
                        name: ActiveValue::set(provider.to_string()),
                        ..Default::default()
                    })
                    .peekable();

                if new_providers.peek().is_some() {
                    provider::Entity::insert_many(new_providers)
                        .exec(&*tx)
                        .await?;
                }

                let all_providers: HashMap<_, _> = provider::Entity::find()
                    .all(&*tx)
                    .await?
                    .iter()
                    .filter_map(|provider| {
                        LanguageModelProvider::from_str(&provider.name)
                            .ok()
                            .map(|p| (p, provider.id))
                    })
                    .collect();

                Ok(all_providers)
            })
            .await?;
        Ok(())
    }

    pub async fn initialize_models(&mut self) -> Result<()> {
        let all_provider_ids = &self.provider_ids;
        self.models = self
            .transaction(|tx| async move {
                let all_models: HashMap<_, _> = model::Entity::find()
                    .all(&*tx)
                    .await?
                    .into_iter()
                    .filter_map(|model| {
                        let provider = all_provider_ids.iter().find_map(|(provider, id)| {
                            if *id == model.provider_id {
                                Some(provider)
                            } else {
                                None
                            }
                        })?;
                        Some(((*provider, model.name.clone()), model))
                    })
                    .collect();
                Ok(all_models)
            })
            .await?;
        Ok(())
    }

    pub async fn insert_models(
        &mut self,
        models: &[(LanguageModelProvider, String, ModelRateLimits)],
    ) -> Result<()> {
        let all_provider_ids = &self.provider_ids;
        self.transaction(|tx| async move {
            model::Entity::insert_many(models.into_iter().map(|(provider, name, rate_limits)| {
                let provider_id = all_provider_ids[&provider];
                model::ActiveModel {
                    provider_id: ActiveValue::set(provider_id),
                    name: ActiveValue::set(name.clone()),
                    max_requests_per_minute: ActiveValue::set(rate_limits.max_requests_per_minute),
                    max_tokens_per_minute: ActiveValue::set(rate_limits.max_tokens_per_minute),
                    max_tokens_per_day: ActiveValue::set(rate_limits.max_tokens_per_day),
                    ..Default::default()
                }
            }))
            .exec_without_returning(&*tx)
            .await?;
            Ok(())
        })
        .await?;
        self.initialize_models().await
    }

    /// Returns the list of LLM providers.
    pub async fn list_providers(&self) -> Result<Vec<LanguageModelProvider>> {
        self.transaction(|tx| async move {
            Ok(provider::Entity::find()
                .order_by_asc(provider::Column::Name)
                .all(&*tx)
                .await?
                .into_iter()
                .filter_map(|p| LanguageModelProvider::from_str(&p.name).ok())
                .collect())
        })
        .await
    }
}
