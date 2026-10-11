// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::AnalyticsError;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};

use crate::analytics_client::GA4AnalyticsClient;
use crate::env_info::{get_arch, get_os, is_googler};
use crate::ga4_event::*;
use crate::metrics_state::*;
use crate::notice::{BRIEF_NOTICE, FULL_NOTICE, GOOGLER_ENHANCED_NOTICE, SHOW_NOTICE_TEMPLATE};

pub(crate) enum WorkerMessage {
    Events(Vec<Event>),
    Flush(futures::channel::oneshot::Sender<()>),
    Drain(futures::channel::oneshot::Sender<()>),
    Stop,
}

struct WorkerHandle {
    sender: futures::channel::mpsc::UnboundedSender<WorkerMessage>,
    abort_handle: futures::future::AbortHandle,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerHandle {
    fn post_events(&self, events: Vec<Event>) {
        if let Err(e) = self.sender.unbounded_send(WorkerMessage::Events(events)) {
            log::warn!("Failed to enqueue analytics events: {e}");
        }
    }

    async fn flush(&self) -> Result<(), AnalyticsError> {
        let (ack_tx, ack_rx) = futures::channel::oneshot::channel();
        if self.sender.unbounded_send(WorkerMessage::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
        Ok(())
    }

    async fn drain(self) -> Result<(), AnalyticsError> {
        let (ack_tx, ack_rx) = futures::channel::oneshot::channel();
        if self.sender.unbounded_send(WorkerMessage::Drain(ack_tx)).is_ok() {
            let _ = ack_rx.await;
        }
        Ok(())
    }

    fn stop(self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        self.abort_handle.abort();
        let _ = self.sender.unbounded_send(WorkerMessage::Stop);
    }
}

/// The implementation of the GA4 Measurement Protocol metrics public api.
//#[derive(Clone)]
pub struct GA4MetricsService {
    metrics_state: MetricsState,
    client: Option<GA4AnalyticsClient>,
    post: Post,
    worker: Option<WorkerHandle>,
}

impl GA4MetricsService {
    pub(crate) fn new(state: MetricsState) -> Self {
        let client = if state.status.is_opted_in() {
            Some(GA4AnalyticsClient::new(state.ga4_key.clone(), state.ga4_product_code.clone()))
        } else {
            None
        };
        let mut svc =
            GA4MetricsService { metrics_state: state, client, post: Post::default(), worker: None };
        svc.init_post();
        if svc.is_opted_in() {
            let _ = svc.start_worker();
        }
        svc
    }

    /// Returns Analytics disclosure notice according to PDD rules.
    pub fn get_notice(&self) -> Option<String> {
        if !is_googler() {
            match self.metrics_state.status {
                MetricsStatus::NewUser => Some(FULL_NOTICE.to_string()),
                MetricsStatus::NewToTool => Some(BRIEF_NOTICE.to_string()),
                _ => None,
            }
        } else {
            match self.metrics_state.status {
                MetricsStatus::GooglerNeedsNotice | MetricsStatus::GooglerOptedInAndNeedsNotice => {
                    if let Some(invoker) = &self.metrics_state.invoker {
                        if invoker != "fx" {
                            Some(GOOGLER_ENHANCED_NOTICE.to_string())
                        } else {
                            None
                        }
                    } else {
                        Some(GOOGLER_ENHANCED_NOTICE.to_string())
                    }
                }
                _ => None,
            }
        }
    }

    pub async fn show_status_message(&self) -> String {
        let optin_status = match self.metrics_state.status {
            MetricsStatus::OptedIn
            | MetricsStatus::GooglerOptedInAndNeedsNotice
            | MetricsStatus::NewToTool => "enabled",
            MetricsStatus::OptedInEnhanced => "enable-enhanced",
            MetricsStatus::OptedOut
            | MetricsStatus::Disabled
            | MetricsStatus::NewUser
            | MetricsStatus::GooglerNeedsNotice => "disabled",
        };
        let message = SHOW_NOTICE_TEMPLATE.replace("{status}", optin_status);
        message
    }

    /// Records Analytics participation status.
    /// TODO remove this once foxtrot is migrated to set_new_opt_in_status
    pub fn set_opt_in_status(&mut self, enabled: bool) -> Result<(), AnalyticsError> {
        self.metrics_state.set_opt_in_status(enabled)?;
        if enabled {
            if self.client.is_none() {
                self.client = Some(GA4AnalyticsClient::new(
                    self.metrics_state.ga4_key.clone(),
                    self.metrics_state.ga4_product_code.clone(),
                ));
            }
            self.start_worker()?;
        } else {
            self.stop_worker();
            self.client = None;
        }
        Ok(())
    }

    /// Record analytics participation status in new migrated status file to support
    /// enhanced analytics for Googlers.
    pub fn set_new_opt_in_status(&mut self, status: MetricsStatus) -> Result<(), AnalyticsError> {
        let is_opted_in = status.is_opted_in();
        self.metrics_state.set_new_opt_in_status(status)?;
        if is_opted_in {
            if self.client.is_none() {
                self.client = Some(GA4AnalyticsClient::new(
                    self.metrics_state.ga4_key.clone(),
                    self.metrics_state.ga4_product_code.clone(),
                ));
            }
            self.start_worker()?;
        } else {
            self.stop_worker();
            self.client = None;
        }
        Ok(())
    }

    pub fn opt_in_status(&self) -> MetricsStatus {
        self.metrics_state.status.clone()
    }

    /// Returns Analytics participation status.
    pub fn is_opted_in(&self) -> bool {
        self.metrics_state.status.is_opted_in()
    }

    /// Disables analytics for this invocation only.
    /// This does not affect the global analytics state.
    pub fn opt_out_for_this_invocation(&mut self) -> Result<(), AnalyticsError> {
        self.stop_worker();
        self.client = None;
        self.metrics_state.opt_out_for_this_invocation()
    }

    fn start_worker(&mut self) -> Result<(), AnalyticsError> {
        if self.worker.is_some() || !self.is_opted_in() {
            return Ok(());
        }
        let (tx, rx) = futures::channel::mpsc::unbounded();
        let (abort_handle, abort_registration) = futures::future::AbortHandle::new_pair();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = std::sync::Arc::clone(&stopped);
        let ga4_key = self.metrics_state.ga4_key.clone();
        let ga4_product_code = self.metrics_state.ga4_product_code.clone();
        let post = self.post.clone();
        match std::thread::Builder::new().name("analytics-worker".to_string()).spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::new();
            let client = GA4AnalyticsClient::new(ga4_key, ga4_product_code);
            let fut = futures::future::Abortable::new(
                run_worker_loop(rx, client, post, worker_stopped),
                abort_registration,
            );
            let _ = executor.run_singlethreaded(fut);
        }) {
            Ok(_thread) => {
                self.worker = Some(WorkerHandle { sender: tx, abort_handle, stopped });
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to spawn analytics worker thread: {e}");
                self.worker = None;
                Err(e.into())
            }
        }
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
        self.post.events.clear();
    }

    fn enqueue_events(&mut self, events: Vec<Event>) -> Result<(), AnalyticsError> {
        if let Some(ref mut worker) = self.worker {
            worker.post_events(events);
        } else {
            for event in events {
                self.post.add_event(event);
            }
        }
        Ok(())
    }

    fn enqueue_event(&mut self, event: Event) -> Result<(), AnalyticsError> {
        self.enqueue_events(vec![event])
    }

    /// Creates a custom GA4 event configured with the service's invoker metadata,
    /// suitable for batching via [`Self::add_events`].
    pub fn make_custom_event(
        &self,
        category: Option<&str>,
        action: Option<&str>,
        label: Option<&str>,
        custom_dimensions: BTreeMap<&str, GA4Value>,
        event_name: Option<&str>,
    ) -> Event {
        make_ga4_event(
            category,
            action,
            label,
            custom_dimensions,
            self.metrics_state.invoker.as_deref(),
            event_name,
        )
    }

    /// Creates a timing GA4 event configured with the service's invoker metadata,
    /// suitable for batching via [`Self::add_events`].
    pub fn make_timing_event(
        &self,
        category: Option<&str>,
        time: u64,
        variable: Option<&str>,
        label: Option<&str>,
        custom_dimensions: BTreeMap<&str, GA4Value>,
    ) -> Event {
        make_ga4_timing_event(
            category,
            time,
            variable,
            label,
            custom_dimensions,
            self.metrics_state.invoker.as_deref(),
        )
    }

    /// Adds a batch of events to the queue
    pub async fn add_events(&mut self, events: Vec<Event>) -> Result<(), AnalyticsError> {
        if !self.is_opted_in() {
            return Ok(());
        }
        self.enqueue_events(events)
    }

    /// Adds a launch event to the queue
    pub async fn add_launch_event(&mut self, args: Option<&str>) -> Result<(), AnalyticsError> {
        self.add_custom_event(None, args, args, BTreeMap::new(), Some("launch")).await
    }

    /// Adds an event to the queue with open-ended parameters
    /// while still honoring the UA Event parameters already
    /// in use.
    pub async fn add_custom_event(
        &mut self,
        category: Option<&str>,
        action: Option<&str>,
        label: Option<&str>,
        custom_dimensions: BTreeMap<&str, GA4Value>,
        event_name: Option<&str>,
    ) -> Result<(), AnalyticsError> {
        if !self.is_opted_in() {
            return Ok(());
        }
        let ga4_event =
            self.make_custom_event(category, action, label, custom_dimensions, event_name);
        self.enqueue_event(ga4_event)
    }

    /// Adds a crash/exception event to the queue
    /// conforming to the UA Event parameters already
    /// in use.
    pub async fn add_crash_event(
        &mut self,
        description: &str,
        fatal: Option<&bool>,
    ) -> Result<(), AnalyticsError> {
        if !self.is_opted_in() {
            return Ok(());
        }
        let ga4_event =
            make_ga4_crash_event(description, fatal, self.metrics_state.invoker.as_deref());
        self.enqueue_event(ga4_event)
    }

    /// Records a timing event from the app.
    pub async fn add_timing_event(
        &mut self,
        category: Option<&str>,
        time: u64,
        variable: Option<&str>,
        label: Option<&str>,
        custom_dimensions: BTreeMap<&str, GA4Value>,
    ) -> Result<(), AnalyticsError> {
        if !self.is_opted_in() {
            return Ok(());
        }
        let ga4_event = self.make_timing_event(category, time, variable, label, custom_dimensions);
        self.enqueue_event(ga4_event)
    }

    async fn flush_local_post(&mut self) -> Result<(), AnalyticsError> {
        if let Some(ref client) = self.client {
            rewrite_ua_ffx_known_batch_to_ga4_post(&mut self.post);
            if !self.post.events.is_empty() {
                let _ = self.post.validate()?;
                client.send(&mut self.post).await?;
                self.post.events.clear();
            }
        }
        Ok(())
    }

    /// Flushes all accumulated events in the background worker queue.
    pub async fn send_events(&mut self) -> Result<(), AnalyticsError> {
        if !self.is_opted_in() {
            return Ok(());
        }
        if let Some(ref worker) = self.worker {
            worker.flush().await?;
        }
        self.flush_local_post().await
    }

    /// Drains all accumulated events in the background worker and terminates the worker thread.
    pub async fn drain(&mut self) -> Result<(), AnalyticsError> {
        if !self.is_opted_in() {
            return Ok(());
        }
        if let Some(worker) = self.worker.take() {
            worker.drain().await?;
        }
        let _ = self.flush_local_post().await;
        Ok(())
    }

    // Send a signal analytics only if the user is a new internal user.
    pub(crate) async fn send_signal_if_new_internal_user(&self) {
        if !self.metrics_state.is_new_internal_user {
            return;
        }
        let Some(uuid) = &self.metrics_state.uuid else {
            log::warn!("UUID is missing when sending signal for new internal user");
            return;
        };
        // For a new user, a full functional metrics service is not ready yet.
        // We need to send the signal with more low level functions.
        let client = GA4AnalyticsClient::new(
            self.metrics_state.ga4_key.clone(),
            self.metrics_state.ga4_product_code.clone(),
        );
        let mut post = Post::new(
            "new_user".into(),
            None,
            None,
            vec![Event::new(
                "new_user".into(),
                Some(Params {
                    items: None,
                    params: HashMap::from([("uuid".to_owned(), uuid.to_string().into())]),
                }),
            )],
        );
        let _ = client.send(&mut post).await;
    }

    fn uuid_as_str(&self) -> String {
        self.metrics_state.uuid.map_or_else(|| "No uuid".to_string(), |u| u.to_string())
    }

    /// Create the GA4 Post object that will be sent to Google Analytics.
    fn init_post(&mut self) {
        self.post = Post::new(self.uuid_as_str(), None, Some(self.make_user_properties()), vec![]);
    }

    /// Initialize the UserProperties to be sent to GA4 with events.
    fn make_user_properties(&self) -> HashMap<String, ValueObject> {
        HashMap::from([
            (
                "build_version".into(),
                ValueObject { value: self.metrics_state.build_version.clone().into() },
            ),
            ("os".into(), ValueObject { value: get_os().into() }),
            ("arch".into(), ValueObject { value: get_arch().into() }),
            (
                "sdk_version".into(),
                ValueObject { value: self.metrics_state.sdk_version.clone().into() },
            ),
            ("internal".into(), ValueObject { value: is_googler_as_int().into() }),
            ("metrics_level".into(), ValueObject { value: self.opted_in_metrics_level().into() }),
        ])
    }

    // This returns which level of opted in is set
    // only when user is opted in.
    // Used to encode level for analytics.
    fn opted_in_metrics_level(&self) -> u64 {
        if self.opt_in_status() == MetricsStatus::OptedInEnhanced { 2 } else { 1 }
    }
}

// encode bool as 1 or 0 for analytics
fn is_googler_as_int() -> u64 {
    match is_googler() {
        true => 1,
        false => 0,
    }
}

impl Drop for GA4MetricsService {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl Default for GA4MetricsService {
    fn default() -> Self {
        let metrics_state = MetricsState::default();
        let client = Some(GA4AnalyticsClient::new(
            metrics_state.ga4_key.clone(),
            metrics_state.ga4_product_code.clone(),
        ));
        Self { metrics_state, client, post: Post::default(), worker: None }
    }
}

async fn send_pending_batches(
    client: &GA4AnalyticsClient,
    post: &mut Post,
    pending: &mut Vec<Event>,
    stopped: &std::sync::atomic::AtomicBool,
) {
    rewrite_ua_ffx_known_batch_events(pending);
    let mut iter = pending.drain(..).peekable();
    while iter.peek().is_some() {
        post.events.clear();
        if stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        post.events.extend(iter.by_ref().take(POST_EVENT_COUNT_MAX));
        if post.validate().is_ok() {
            let _ = client.send(post).await;
        } else {
            post.events.clear();
        }
    }
}

fn rewrite_ua_ffx_known_batch_events(events: &mut Vec<Event>) {
    let mut i = 0;
    while i + 1 < events.len() {
        if events[i].name.eq_ignore_ascii_case("invoke")
            && events[i + 1].name.eq_ignore_ascii_case("timing")
        {
            log::trace!("Rewriting ffx batch invoke to ga4 post invoke");
            let timing_event = events.remove(i + 1);
            if let Some(params) = timing_event.params {
                if let Some(time) = params.params.get("time").cloned() {
                    events[i].add_param("timing", time);
                }
            }
        } else {
            i += 1;
        }
    }
}

fn rewrite_ua_ffx_known_batch_to_ga4_post(post: &mut Post) {
    rewrite_ua_ffx_known_batch_events(&mut post.events);
}

async fn run_worker_loop(
    mut rx: futures::channel::mpsc::UnboundedReceiver<WorkerMessage>,
    client: GA4AnalyticsClient,
    mut post: Post,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut pending_events: Vec<Event> = Vec::new();

    while let Some(msg) = rx.next().await {
        if stopped.load(std::sync::atomic::Ordering::SeqCst) {
            pending_events.clear();
            return;
        }
        match msg {
            WorkerMessage::Events(events) => {
                pending_events.extend(events);
                while let Ok(next_msg) = rx.try_recv() {
                    if stopped.load(std::sync::atomic::Ordering::SeqCst) {
                        pending_events.clear();
                        return;
                    }
                    match next_msg {
                        WorkerMessage::Events(e) => pending_events.extend(e),
                        WorkerMessage::Flush(ack) => {
                            send_pending_batches(&client, &mut post, &mut pending_events, &stopped)
                                .await;
                            let _ = ack.send(());
                        }
                        WorkerMessage::Drain(ack) => {
                            send_pending_batches(&client, &mut post, &mut pending_events, &stopped)
                                .await;
                            let _ = ack.send(());
                            return;
                        }
                        WorkerMessage::Stop => {
                            pending_events.clear();
                            return;
                        }
                    }
                }
                send_pending_batches(&client, &mut post, &mut pending_events, &stopped).await;
            }
            WorkerMessage::Flush(ack) => {
                send_pending_batches(&client, &mut post, &mut pending_events, &stopped).await;
                let _ = ack.send(());
            }
            WorkerMessage::Drain(ack) => {
                send_pending_batches(&client, &mut post, &mut pending_events, &stopped).await;
                let _ = ack.send(());
                return;
            }
            WorkerMessage::Stop => {
                pending_events.clear();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    const APP_NAME: &str = "my cool app";
    const BUILD_VERSION: &str = "12/09/20 00:00:00";
    const SDK_VERSION: &str = "99.99.99.99.1";
    // const LAUNCH_ARGS: &str = "config analytics enable";

    fn test_metrics_svc(
        app_support_dir_path: &PathBuf,
        app_name: String,
        build_version: String,
        sdk_version: String,
        ga_product_code: String,
        ga4_product_code: String,
        ga4_key: String,
        disabled: bool,
    ) -> GA4MetricsService {
        GA4MetricsService::new(MetricsState::from_config(
            app_support_dir_path,
            app_name,
            build_version,
            sdk_version,
            ga_product_code,
            ga4_product_code,
            ga4_key,
            disabled,
            None,
        ))
    }

    #[test]
    fn new_user_of_any_tool() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        let ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        if !is_googler() {
            assert_eq!(ms.get_notice(), Some(FULL_NOTICE.into()));
        } else {
            assert_eq!(ms.get_notice(), Some(GOOGLER_ENHANCED_NOTICE.into()));
        }

        drop(dir);
        Ok(())
    }

    #[test]
    fn existing_user_first_use_of_this_tool() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, true)?;

        let ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );
        if !is_googler() {
            assert_eq!(ms.metrics_state.status, MetricsStatus::NewToTool);
        } else {
            assert_eq!(ms.metrics_state.status, MetricsStatus::GooglerOptedInAndNeedsNotice);
        }
        if !is_googler() {
            assert_eq!(ms.get_notice(), Some(BRIEF_NOTICE.into()));
        } else {
            assert_eq!(ms.get_notice(), Some(GOOGLER_ENHANCED_NOTICE.into()));
        }
        drop(dir);
        Ok(())
    }

    #[test]
    fn existing_user_of_this_tool_opted_in() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, true)?;
        write_app_status(&dir, &APP_NAME, true)?;
        let ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        if !is_googler() {
            assert_eq!(ms.get_notice(), None);
        } else {
            assert_eq!(ms.get_notice(), Some(GOOGLER_ENHANCED_NOTICE.into()));
        }
        drop(dir);
        Ok(())
    }

    #[test]
    fn existing_user_of_this_tool_opted_out() -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, false)?;
        write_app_status(&dir, &APP_NAME, true)?;
        let ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        assert_eq!(ms.get_notice(), None);

        drop(dir);
        Ok(())
    }

    #[test]
    fn with_disable_env_var_set() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, true)?;
        write_app_status(&dir, &APP_NAME, true)?;

        let ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            true,
        );

        assert_eq!(ms.get_notice(), None);

        drop(dir);
        Ok(())
    }

    #[test]
    fn opt_out_for_this_invocation() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        let mut ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        if !is_googler() {
            assert_eq!(ms.metrics_state.status, MetricsStatus::NewUser);
        } else {
            assert_eq!(ms.metrics_state.status, MetricsStatus::GooglerNeedsNotice);
        }
        let _res = ms.opt_out_for_this_invocation().unwrap();
        assert_eq!(ms.metrics_state.status, MetricsStatus::OptedOut);

        drop(dir);
        Ok(())
    }

    #[test]
    fn opt_in_from_opted_out() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, false)?;
        let mut ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );
        assert!(!ms.metrics_state.status.is_opted_in());
        assert!(ms.client.is_none());
        ms.set_new_opt_in_status(MetricsStatus::OptedIn).unwrap();
        assert!(ms.metrics_state.status.is_opted_in(), "{:?}", ms.metrics_state.status);
        assert!(ms.client.is_some());

        drop(dir);
        Ok(())
    }

    #[test]
    fn opt_out_from_opted_in() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, true)?;
        let mut ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        assert!(ms.metrics_state.status.is_opted_in(), "{:?}", ms.metrics_state.status);
        assert!(ms.client.is_some());
        ms.set_new_opt_in_status(MetricsStatus::OptedOut).unwrap();
        assert!(!ms.metrics_state.status.is_opted_in());
        assert!(ms.client.is_none());

        drop(dir);
        Ok(())
    }

    #[test]
    fn opt_in_from_opted_in() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, true)?;
        let mut ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        assert!(ms.metrics_state.status.is_opted_in(), "{:?}", ms.metrics_state.status);
        assert!(ms.client.is_some());
        ms.set_new_opt_in_status(MetricsStatus::OptedIn).unwrap();
        assert!(ms.metrics_state.status.is_opted_in());
        assert!(ms.client.is_some());

        drop(dir);
        Ok(())
    }

    #[test]
    fn opt_out_from_opted_out() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, false)?;
        let mut ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );

        assert!(!ms.metrics_state.status.is_opted_in(), "{:?}", ms.metrics_state.status);
        assert!(ms.client.is_none());
        ms.set_new_opt_in_status(MetricsStatus::OptedOut).unwrap();
        assert!(!ms.metrics_state.status.is_opted_in());
        assert!(ms.client.is_none());

        drop(dir);
        Ok(())
    }

    #[test]
    fn test_rewrite_ua_ffx_known_batch() {
        let mut events = vec![
            Event::new("ffx_connection_mode".to_string(), None),
            Event::new("invoke".to_string(), None),
            Event::new(
                "timing".to_string(),
                Some(Params {
                    items: None,
                    params: HashMap::from([(
                        "time".to_string(),
                        crate::ga4_event::GA4Value::Str("42".to_string()),
                    )]),
                }),
            ),
        ];
        rewrite_ua_ffx_known_batch_events(&mut events);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "ffx_connection_mode");
        assert_eq!(events[1].name, "invoke");
        let params = events[1].params.as_ref().unwrap();
        assert_eq!(
            params.params.get("timing"),
            Some(&crate::ga4_event::GA4Value::Str("42".to_string()))
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_drain_and_stop_worker() -> Result<(), AnalyticsError> {
        let dir = create_tmp_metrics_dir()?;
        write_opt_in_status(&dir, true)?;
        write_app_status(&dir, &APP_NAME, true)?;
        let mut ms = test_metrics_svc(
            &dir,
            String::from(APP_NAME),
            String::from(BUILD_VERSION),
            String::from(SDK_VERSION),
            UNKNOWN_PROPERTY_ID.to_string(),
            UNKNOWN_GA4_PRODUCT_CODE.to_string(),
            UNKNOWN_GA4_KEY.to_string(),
            false,
        );
        assert!(ms.worker.is_some());
        ms.drain().await?;
        assert!(ms.worker.is_none());

        ms.start_worker()?;
        assert!(ms.worker.is_some());
        ms.stop_worker();
        assert!(ms.worker.is_none());

        drop(dir);
        Ok(())
    }

    pub fn create_tmp_metrics_dir() -> Result<PathBuf, AnalyticsError> {
        let tmp_dir = tempdir()?;
        let dir_obj = tmp_dir.path().join("fuchsia_metrics");
        let dir = dir_obj.as_path();
        Ok(dir.to_owned())
    }
}
