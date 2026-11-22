// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use crate::common::suspend_request_error;
use anyhow::{Context, Result, bail};
use args::SuspendCommand;
#[cfg(feature = "fdomain")]
use flex_client::AsHandleRef;
use flex_client::ProxyHasDomain;
use flex_fuchsia_driver_development as fdd;
#[cfg(feature = "fdomain")]
use fuchsia_driver_dev_fdomain as fuchsia_driver_dev;
use futures::future::{Either, Future, pending, select};
use futures::pin_mut;
use std::io::Write;
use zx_status::Status;

fn parse_target_args(cmd: &SuspendCommand) -> Result<(String, Option<String>)> {
    let (moniker, suffix) = match cmd.moniker.split_once('#') {
        Some((moniker_part, disambiguator)) => {
            if disambiguator.is_empty() {
                bail!("Disambiguator after '#' must not be empty.");
            }
            (moniker_part.to_string(), Some(disambiguator.to_string()))
        }
        None => (cmd.moniker.clone(), None),
    };
    if moniker.is_empty() {
        bail!("Target moniker must not be empty.");
    }
    if cmd.disambiguator.as_deref().is_some_and(str::is_empty) {
        bail!("Disambiguator passed to '--disambiguator' must not be empty.");
    }
    let disambiguator = match (suffix, cmd.disambiguator.as_ref()) {
        (Some(suffix), Some(flag)) if !suffix.eq_ignore_ascii_case(flag) => {
            bail!(
                "Conflicting disambiguators provided in moniker ('{suffix}') and --disambiguator \
                 ('{flag}')."
            );
        }
        (Some(suffix), _) => Some(suffix),
        (None, Some(flag)) => Some(flag.clone()),
        (None, None) => None,
    };
    Ok((moniker, disambiguator))
}

fn format_target_label(moniker: &str, disambiguator: Option<&str>) -> String {
    match disambiguator {
        Some(disambiguator) => format!("{moniker}#{disambiguator}"),
        None => moniker.to_string(),
    }
}

pub async fn suspend(
    cmd: SuspendCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    suspend_impl(cmd, writer, driver_development_proxy, pending()).await
}

async fn suspend_impl(
    cmd: SuspendCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
    wait_for_interrupt: impl Future<Output = ()>,
) -> Result<()> {
    let (moniker, disambiguator) = parse_target_args(&cmd)?;
    let target_label = format_target_label(&moniker, disambiguator.as_deref());

    let mut candidates = fuchsia_driver_dev::get_device_info(
        &driver_development_proxy,
        std::slice::from_ref(&moniker),
        true,
    )
    .await?;

    if let Some(expected_disambiguator) = disambiguator.as_deref() {
        candidates.retain(|node| {
            node.disambiguator.as_deref().unwrap_or("").eq_ignore_ascii_case(expected_disambiguator)
        });
    }

    let chosen = match <[_; 1]>::try_from(candidates) {
        Ok([chosen]) => chosen,
        Err(candidates) if candidates.is_empty() => {
            return Err(suspend_request_error(&target_label, Status::NOT_FOUND));
        }
        Err(candidates) => {
            let mut message = format!(
                "Multiple nodes match moniker '{moniker}'. Re-run with --disambiguator (or \
                 '<moniker>#<disambiguator>'):\n"
            );
            for node in candidates {
                let node_disambiguator = node.disambiguator.as_deref().unwrap_or("<none>");
                let owner = node.bound_driver_url.as_deref().unwrap_or("<unbound>");
                message.push_str(&format!(
                    "  - Moniker: {moniker}, Disambiguator: {node_disambiguator}, Owner: {owner}\n"
                ));
            }
            bail!("{}", message.trim_end());
        }
    };

    let target = fdd::NodeTarget {
        moniker: Some(moniker),
        disambiguator: chosen.disambiguator,
        ..Default::default()
    };

    writeln!(writer, "Suspending {target_label}.")?;
    writer.flush()?;

    let (client_token, server_token) = driver_development_proxy.domain().create_event_pair();

    driver_development_proxy
        .start_suspend_session(&target, server_token)
        .await?
        .map_err(|err| suspend_request_error(&target_label, Status::err_from_raw(err)))?;

    writeln!(writer, "Suspended driver successfully.")?;
    writer.flush()?;

    {
        let client_handle = client_token.as_handle_ref();
        let peer_closed =
            flex_client::wait_for_signals(&client_handle, fidl::Signals::OBJECT_PEER_CLOSED);
        pin_mut!(wait_for_interrupt, peer_closed);
        match select(wait_for_interrupt, peer_closed).await {
            Either::Left(((), _)) => {}
            Either::Right((result, _)) => {
                result.context("Failed while waiting for suspend session to end")?;
                writeln!(writer, "Suspend session ended by driver manager.")?;
                writer.flush()?;
            }
        }
    }

    drop(client_token);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;
    use flex_client::fidl::ServerEnd;
    use fuchsia_async as fasync;
    use fuchsia_sync::Mutex;
    use futures::channel::oneshot;
    use futures::future::{FutureExt, poll_fn, ready};
    use futures::stream::{StreamExt, TryStreamExt};
    use std::sync::Arc;

    fn test_node_info() -> fdd::NodeInfo {
        fdd::NodeInfo {
            moniker: Some("dev.sys.test.node".to_string()),
            disambiguator: Some("0123456789abcdef".to_string()),
            ..Default::default()
        }
    }

    async fn serve_node_info(
        iterator: ServerEnd<fdd::NodeInfoIteratorMarker>,
        nodes: Vec<fdd::NodeInfo>,
    ) {
        let mut stream = iterator.into_stream();
        let mut sent = false;
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fdd::NodeInfoIteratorRequest::GetNext { responder } => {
                    if sent {
                        let _ = responder.send(&[]);
                    } else {
                        sent = true;
                        let _ = responder.send(&nodes);
                    }
                }
            }
        }
    }

    /// Invokes `suspend_impl` with `cmd` and `wait_for_interrupt`, running a mock
    /// driver development server that invokes `on_driver_development_request`
    /// whenever it receives a request. Returns the formatted `writer` output.
    async fn test_suspend_with_wait<F, Fut>(
        #[cfg(feature = "fdomain")] client: Arc<flex_client::Client>,
        cmd: SuspendCommand,
        wait_for_interrupt: impl Future<Output = ()>,
        on_driver_development_request: F,
    ) -> Result<String>
    where
        F: Fn(fdd::ManagerRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + Sync,
    {
        #[cfg(not(feature = "fdomain"))]
        let client = flex_client::fidl::ZirconClient;
        let (driver_development_proxy, mut driver_development_requests) =
            client.create_proxy_and_stream::<fdd::ManagerMarker>();

        // Run the command and mock driver development server.
        let mut writer = Vec::new();
        let request_handler_task = fasync::Task::spawn(async move {
            while let Some(result) = driver_development_requests.next().await {
                let request = result.context("Failed to get driver development request")?;
                on_driver_development_request(request).await.context("Failed to handle request")?;
            }
            anyhow::bail!("Driver development request stream unexpectedly closed");
        });
        futures::select! {
            result = request_handler_task.fuse() => {
                result?;
                anyhow::bail!("Request handler task unexpectedly finished");
            }
            result = suspend_impl(cmd, &mut writer, driver_development_proxy, wait_for_interrupt).fuse() => result?,
        }

        String::from_utf8(writer).context("Failed to convert suspend output to a string")
    }

    async fn test_suspend<F, Fut>(
        #[cfg(feature = "fdomain")] client: Arc<flex_client::Client>,
        cmd: SuspendCommand,
        on_driver_development_request: F,
    ) -> Result<String>
    where
        F: Fn(fdd::ManagerRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + Sync,
    {
        test_suspend_with_wait(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            ready(()),
            on_driver_development_request,
        )
        .await
    }

    async fn test_suspend_nodes_only(
        #[cfg(feature = "fdomain")] client: Arc<flex_client::Client>,
        cmd: SuspendCommand,
        nodes: Vec<fdd::NodeInfo>,
    ) -> Result<String> {
        test_suspend(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            move |request: fdd::ManagerRequest| {
                let nodes = nodes.clone();
                async move {
                    if let fdd::ManagerRequest::GetNodeInfo { iterator, .. } = request {
                        serve_node_info(iterator, nodes).await;
                    }
                    Ok(())
                }
            },
        )
        .await
    }

    async fn test_suspend_selects_target(
        #[cfg(feature = "fdomain")] client: Arc<flex_client::Client>,
        cmd: SuspendCommand,
        nodes: Vec<fdd::NodeInfo>,
        expected_moniker: &'static str,
        expected_disambiguator: &'static str,
    ) -> Result<String> {
        let held_tokens = Arc::new(Mutex::new(Vec::new()));
        let held_tokens_clone = Arc::clone(&held_tokens);
        test_suspend(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            move |request: fdd::ManagerRequest| {
                let nodes = nodes.clone();
                let held_tokens = Arc::clone(&held_tokens_clone);
                async move {
                    match request {
                        fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                            serve_node_info(iterator, nodes).await;
                        }
                        fdd::ManagerRequest::StartSuspendSession { target, token, responder } => {
                            assert_eq!(target.moniker.as_deref(), Some(expected_moniker));
                            assert_eq!(
                                target.disambiguator.as_deref(),
                                Some(expected_disambiguator)
                            );
                            held_tokens.lock().push(token);
                            responder.send(Ok(())).unwrap();
                        }
                        _ => {}
                    }
                    Ok(())
                }
            },
        )
        .await
    }

    #[fuchsia::test]
    async fn test_suspend_driver() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        let cmd = SuspendCommand::from_args(&["suspend"], &["dev.sys.test.node"]).unwrap();
        let held_tokens = Arc::new(Mutex::new(Vec::new()));
        let held_tokens_clone = Arc::clone(&held_tokens);

        let output = test_suspend(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            move |request: fdd::ManagerRequest| {
                let held_tokens = Arc::clone(&held_tokens_clone);
                async move {
                    match request {
                        fdd::ManagerRequest::GetNodeInfo {
                            node_filter,
                            iterator,
                            exact_match,
                            ..
                        } => {
                            assert_eq!(node_filter, vec!["dev.sys.test.node".to_string()]);
                            assert!(exact_match);
                            serve_node_info(iterator, vec![test_node_info()]).await;
                        }
                        fdd::ManagerRequest::StartSuspendSession { target, token, responder } => {
                            assert_eq!(target.moniker.as_deref(), Some("dev.sys.test.node"));
                            assert_eq!(target.disambiguator.as_deref(), Some("0123456789abcdef"));
                            held_tokens.lock().push(token);
                            responder.send(Ok(())).unwrap();
                        }
                        _ => {}
                    }
                    Ok(())
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(output, "Suspending dev.sys.test.node.\nSuspended driver successfully.\n");
    }

    #[fuchsia::test]
    async fn test_suspend_holds_token_until_interrupted() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        #[cfg(not(feature = "fdomain"))]
        let client = flex_client::fidl::ZirconClient;
        let (driver_development_proxy, mut driver_development_requests) =
            client.create_proxy_and_stream::<fdd::ManagerMarker>();

        let held_token = Arc::new(Mutex::new(None));
        let held_token_clone = Arc::clone(&held_token);
        let server_task = fasync::Task::spawn(async move {
            while let Some(Ok(request)) = driver_development_requests.next().await {
                match request {
                    fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                        serve_node_info(iterator, vec![test_node_info()]).await;
                    }
                    fdd::ManagerRequest::StartSuspendSession { target: _, token, responder } => {
                        *held_token_clone.lock() = Some(token);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {}
                }
            }
        });

        let (interrupt_tx, mut interrupt_rx) = oneshot::channel::<()>();
        let (polled_tx, polled_rx) = oneshot::channel::<()>();
        let mut polled_tx = Some(polled_tx);
        let wait_for_interrupt = poll_fn(move |cx| {
            if let Some(tx) = polled_tx.take() {
                let _ = tx.send(());
            }
            interrupt_rx.poll_unpin(cx).map(|_| ())
        });

        let cmd = SuspendCommand::from_args(&["suspend"], &["dev.sys.test.node"]).unwrap();
        let mut writer = Vec::new();
        {
            let suspend_future =
                suspend_impl(cmd, &mut writer, driver_development_proxy, wait_for_interrupt).fuse();
            pin_mut!(suspend_future);

            // Drive suspend_impl until it reaches the interrupt wait after StartSuspendSession.
            futures::select! {
                result = suspend_future => {
                    panic!("suspend_impl exited unexpectedly before interrupt: {result:?}")
                }
                _ = polled_rx.fuse() => {}
            }

            // Token must remain open while suspended.
            let server_token = held_token.lock().take().expect("server token should be held");
            let server_handle = server_token.as_handle_ref();
            let mut peer_closed = Box::pin(
                flex_client::wait_for_signals(&server_handle, fidl::Signals::OBJECT_PEER_CLOSED)
                    .fuse(),
            );
            assert!(futures::poll!(peer_closed.as_mut()).is_pending());

            // Interrupting resumes and closes client_token.
            interrupt_tx.send(()).unwrap();
            suspend_future.await.unwrap();
            server_task.await;

            let signals = peer_closed.await.unwrap();
            assert!(signals.contains(fidl::Signals::OBJECT_PEER_CLOSED));
        }

        assert_eq!(
            String::from_utf8(writer).unwrap(),
            "Suspending dev.sys.test.node.\nSuspended driver successfully.\n"
        );
    }

    #[fuchsia::test]
    async fn test_suspend_driver_disambiguates_with_disambiguator() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();

        let make_nodes = || {
            vec![
                fdd::NodeInfo {
                    moniker: Some("dev.sys.dup".to_string()),
                    disambiguator: Some("1111111111111111".to_string()),
                    bound_driver_url: Some("fuchsia-boot:///a#meta/a.cm".to_string()),
                    ..Default::default()
                },
                fdd::NodeInfo {
                    moniker: Some("dev.sys.dup".to_string()),
                    disambiguator: Some("2222222222222222".to_string()),
                    bound_driver_url: Some("fuchsia-boot:///b#meta/b.cm".to_string()),
                    ..Default::default()
                },
            ]
        };

        // Ambiguous without disambiguator prints candidate details.
        let cmd_ambiguous = SuspendCommand::from_args(&["suspend"], &["dev.sys.dup"]).unwrap();
        let err = test_suspend_nodes_only(
            #[cfg(feature = "fdomain")]
            client.clone(),
            cmd_ambiguous,
            make_nodes(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("Multiple nodes match moniker 'dev.sys.dup'"), "{err}");
        assert!(
            err.contains("Re-run with --disambiguator (or '<moniker>#<disambiguator>'):"),
            "{err}"
        );
        assert!(!err.contains("--bus"), "{err}");
        assert!(!err.contains("Bus Topology"), "{err}");
        assert!(err.contains("1111111111111111"), "{err}");
        assert!(err.contains("2222222222222222"), "{err}");

        // Disambiguating with `--disambiguator` picks node 2.
        let cmd_flag = SuspendCommand::from_args(
            &["suspend"],
            &["dev.sys.dup", "--disambiguator", "2222222222222222"],
        )
        .unwrap();
        let output_flag = test_suspend_selects_target(
            #[cfg(feature = "fdomain")]
            client.clone(),
            cmd_flag,
            make_nodes(),
            "dev.sys.dup",
            "2222222222222222",
        )
        .await
        .unwrap();
        assert_eq!(
            output_flag,
            "Suspending dev.sys.dup#2222222222222222.\nSuspended driver successfully.\n"
        );

        // Disambiguating with `<moniker>#<disambiguator>` picks node 1.
        let cmd_hash =
            SuspendCommand::from_args(&["suspend"], &["dev.sys.dup#1111111111111111"]).unwrap();
        let output_hash = test_suspend_selects_target(
            #[cfg(feature = "fdomain")]
            client.clone(),
            cmd_hash,
            make_nodes(),
            "dev.sys.dup",
            "1111111111111111",
        )
        .await
        .unwrap();
        assert_eq!(
            output_hash,
            "Suspending dev.sys.dup#1111111111111111.\nSuspended driver successfully.\n"
        );

        // Mismatched `--disambiguator` when matching-moniker nodes exist returns NOT_FOUND.
        let cmd_mismatch = SuspendCommand::from_args(
            &["suspend"],
            &["dev.sys.dup", "--disambiguator", "9999999999999999"],
        )
        .unwrap();
        let err = test_suspend_nodes_only(
            #[cfg(feature = "fdomain")]
            client.clone(),
            cmd_mismatch,
            make_nodes(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("(NOT_FOUND)"), "{err}");
    }

    #[fuchsia::test]
    async fn test_suspend_exits_when_server_drops_token() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        let cmd = SuspendCommand::from_args(&["suspend"], &["dev.sys.test.node"]).unwrap();

        let output = test_suspend_with_wait(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            pending(),
            |request: fdd::ManagerRequest| async move {
                match request {
                    fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                        serve_node_info(iterator, vec![test_node_info()]).await;
                    }
                    fdd::ManagerRequest::StartSuspendSession { target: _, token, responder } => {
                        drop(token);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {}
                }
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(
            output,
            "Suspending dev.sys.test.node.\nSuspended driver successfully.\nSuspend session ended \
             by driver manager.\n"
        );
    }

    #[fuchsia::test]
    fn test_parse_target_args_validation() {
        let empty_moniker = SuspendCommand::from_args(&["suspend"], &[""]).unwrap();
        assert!(parse_target_args(&empty_moniker).is_err());

        let empty_prefix_with_hash = SuspendCommand::from_args(&["suspend"], &["#abc"]).unwrap();
        assert!(parse_target_args(&empty_prefix_with_hash).is_err());

        let empty_hash_suffix = SuspendCommand::from_args(&["suspend"], &["dev.sys#"]).unwrap();
        assert!(parse_target_args(&empty_hash_suffix).is_err());

        let empty_flag =
            SuspendCommand::from_args(&["suspend"], &["dev.sys", "--disambiguator", ""]).unwrap();
        assert!(parse_target_args(&empty_flag).is_err());

        let conflicting = SuspendCommand::from_args(
            &["suspend"],
            &["dev.sys#1111111111111111", "--disambiguator", "2222222222222222"],
        )
        .unwrap();
        assert!(parse_target_args(&conflicting).is_err());

        let case_insensitive_match = SuspendCommand::from_args(
            &["suspend"],
            &["dev.sys#aabbccdd00112233", "--disambiguator", "AABBCCDD00112233"],
        )
        .unwrap();
        let (moniker, disambiguator) = parse_target_args(&case_insensitive_match).unwrap();
        assert_eq!(moniker, "dev.sys");
        assert_eq!(disambiguator.as_deref(), Some("aabbccdd00112233"));
    }

    #[fuchsia::test]
    async fn test_suspend_driver_case_insensitive_disambiguator() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        let cmd = SuspendCommand::from_args(
            &["suspend"],
            &["dev.sys.dup", "--disambiguator", "AABBCCDD00112233"],
        )
        .unwrap();

        let nodes = vec![
            fdd::NodeInfo {
                moniker: Some("dev.sys.dup".to_string()),
                disambiguator: Some("1111111111111111".to_string()),
                ..Default::default()
            },
            fdd::NodeInfo {
                moniker: Some("dev.sys.dup".to_string()),
                disambiguator: Some("aabbccdd00112233".to_string()),
                ..Default::default()
            },
        ];

        let output = test_suspend_selects_target(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            nodes,
            "dev.sys.dup",
            "aabbccdd00112233",
        )
        .await
        .unwrap();

        assert_eq!(
            output,
            "Suspending dev.sys.dup#AABBCCDD00112233.\nSuspended driver successfully.\n"
        );
    }

    #[fuchsia::test]
    async fn test_suspend_driver_not_found() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        let cmd = SuspendCommand::from_args(&["suspend"], &["dev.sys.test.node"]).unwrap();

        let err = test_suspend_nodes_only(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            vec![],
        )
        .await
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Failed to suspend dev.sys.test.node: no matching node or power dependency was found \
             (NOT_FOUND)"
        );
    }

    #[fuchsia::test]
    async fn test_suspend_session_exists() {
        #[cfg(feature = "fdomain")]
        let client = fdomain_local::local_client_empty();
        let cmd = SuspendCommand::from_args(&["suspend"], &["dev.sys.test.node"]).unwrap();

        let err = test_suspend(
            #[cfg(feature = "fdomain")]
            client,
            cmd,
            |request: fdd::ManagerRequest| async move {
                match request {
                    fdd::ManagerRequest::GetNodeInfo { iterator, .. } => {
                        serve_node_info(iterator, vec![test_node_info()]).await;
                    }
                    fdd::ManagerRequest::StartSuspendSession { target: _, token: _, responder } => {
                        responder.send(Err(Status::ALREADY_EXISTS.into_raw())).unwrap();
                    }
                    _ => {}
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Failed to suspend dev.sys.test.node: a suspend session is already active for this \
             driver (ALREADY_EXISTS)"
        );
    }
}
