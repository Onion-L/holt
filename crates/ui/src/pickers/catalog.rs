//! Provider/model/ref catalog loading for the pickers: the idempotent
//! `ensure_*` kicks the render loop and popover opens drive, the
//! stale-while-revalidate behavior that keeps loaded rows visible during
//! refreshes, and the retry guard around model discovery. State lives on the
//! shared [`super::Pickers`] entity; this module only moves it.

use std::time::Duration;

use gpui::Context;

use holt_proto::{Model, Provider, ProviderId, RepoRef};
use holt_rpc::methods;

use crate::popover::Loadable;

use super::{PickerKind, normalize_model_rows, offered_providers, slow_catalog_delay};

use super::Pickers;

impl Pickers {
    // ---- loads ----

    pub(super) fn ensure_providers(&mut self, force: bool, cx: &mut Context<Self>) {
        // Non-forced (the render loop's eager kick) only loads from Idle: an
        // Error that could re-trigger a load would flip back to Loading
        // before the retry row ever painted (and spam the engine); Retry
        // resets to Idle. Forced refreshes reload through Ready/Error too so
        // provider credential changes do not leave the boot-time catalog
        // cached until restart. Loaded rows stay visible during the refresh.
        let reload = match self.providers {
            Loadable::Idle => true,
            Loadable::Loading => false,
            Loadable::Ready(_) | Loadable::Error(_) => force,
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if !matches!(self.providers, Loadable::Ready(_)) {
            self.providers = Loadable::Loading;
            self.catalog_rev += 1;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_PROVIDERS, serde_json::json!({}))
                .await;
            if let Some(delay) = slow_catalog_delay() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |pickers, cx| {
                pickers.catalog_rev += 1;
                pickers.providers = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Provider>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                pickers.prefetch_models(false, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Kick a model load for the effective provider AND every offered one, in
    /// parallel — by the time the user opens the picker (or switches rail
    /// tabs) the lists are already there, instead of a per-selection
    /// "Loading models…" round-trip. Each `ensure_models` call is guarded by
    /// its slot state, so re-running this every catalog load/render is free.
    pub(super) fn prefetch_models(&mut self, force: bool, cx: &mut Context<Self>) {
        let mut targets: Vec<ProviderId> = match self.providers.ready() {
            Some(list) => offered_providers(list)
                .iter()
                .map(|d| d.id.clone())
                .collect(),
            None => Vec::new(),
        };
        // The committed chat's provider may be outside the offered set (e.g.
        // disabled after the chat was created) — its models still matter.
        if let Some(effective) = self.effective_provider(cx)
            && !targets.contains(&effective)
        {
            targets.push(effective);
        }
        for provider in targets {
            self.ensure_models(provider, force, cx);
        }
    }

    pub(super) fn ensure_models(
        &mut self,
        provider: ProviderId,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        // Normal prefetches load absent/Idle slots once. Picker-open refreshes
        // also retry Ready/Error slots, while an in-flight load is always
        // reused. Ready rows stay visible until the replacement lands.
        let reload = match self.models.get(&provider) {
            None | Some(Loadable::Idle) => true,
            Some(Loadable::Loading) => false,
            Some(Loadable::Ready(_)) | Some(Loadable::Error(_)) => force,
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if !matches!(self.models.get(&provider), Some(Loadable::Ready(_))) {
            self.models.insert(provider.clone(), Loadable::Loading);
            self.catalog_rev += 1;
        }
        cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "providerId": provider });
            // A plugin-heavy OpenCode cold start can fail once while caches,
            // MCP servers, or plugin runtimes are still warming. Keep this
            // single Loading slot alive for two retries so recovery requires
            // no picker close/reopen and cannot launch duplicate probes.
            let mut attempt = 1_u64;
            let result = loop {
                let result = engine
                    .client()
                    .call(methods::LIST_MODELS, params.clone())
                    .await;
                if result.is_ok() || attempt >= 1 {
                    break result;
                }
                if let Err(error) = &result {
                    tracing::warn!(
                        %error,
                        attempt,
                        "OpenCode model discovery failed; retrying automatically"
                    );
                }
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(attempt * 2))
                    .await;
                attempt += 1;
            };
            if let Some(delay) = slow_catalog_delay() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |pickers, cx| {
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                        // Display hygiene for catalogs from older engines
                        // (`default` alias rows, orphan `[1m]` variants,
                        // version-less alias labels).
                        Ok(models) => Loadable::Ready(normalize_model_rows(models)),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let Loadable::Ready(models) = &loaded {
                    let fresh = pickers
                        .defaults
                        .remember_labels(models.iter().map(|m| (m.id.as_str(), m.label.as_str())));
                    if fresh {
                        pickers.save_defaults();
                    }
                }
                pickers.models.insert(provider.clone(), loaded);
                pickers.catalog_rev += 1;
                // A list that landed while its popover is open re-anchors the
                // keyboard highlight onto the selected row (it sat at 0 while
                // loading).
                if pickers.open_kind() == Some(PickerKind::ProviderModel)
                    && pickers.effective_provider(cx) == Some(provider)
                {
                    pickers.active = pickers.selected_model_index(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// ListRefs for the selected SPACE's folder, keyed/invalidated by space
    /// id. Rows carry checkout state (`current`, `worktreePath`) so the
    /// picker can tag refs and the checkout-kind selector can offer worktree
    /// reuse.
    pub(super) fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_deref() == Some(space.id.as_str());
        if fresh && matches!(self.refs, Loadable::Loading) {
            return; // a load is already in flight
        }
        // Non-forced (the footer's eager kick, re-run every render) only loads
        // from Idle: an Error must WAIT for an explicit retry/reopen (force),
        // or re-render would flip Error back to Loading before the retry row
        // ever paints — an eternal skeleton plus an RPC storm (user report:
        // "the ref dropdown never loads anything").
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        // Stale-while-revalidate: a forced refresh of an already-loaded space
        // keeps the current rows on screen while the reload runs — a send that
        // just minted a worktree (or a terminal-side branch) appears on the
        // popover's next open without the list ever flashing to a skeleton.
        if !(force && fresh && matches!(self.refs, Loadable::Ready(_))) {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space.id.clone());
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            let result = engine
                .client()
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.refs = match result {
                    Ok(value) => match serde_json::from_value::<Vec<RepoRef>>(value) {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                // Rows landed under an open, un-searched popover: re-home the
                // nav highlight to the selected row.
                if pickers.open_kind() == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }
}
