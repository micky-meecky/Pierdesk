#![cfg_attr(windows, windows_subsystem = "windows")]

use std::fmt::Write as _;
#[cfg(feature = "licensing")]
use std::io::{BufRead as _, Read as _};
use std::process::ExitCode;
use std::str::FromStr as _;
use std::time::Duration;

use pierdesk::app_settings::AppSettings;
use pierdesk::clipboard::secure_transfer::{SecureOfferClient, SecureRecoveryPolicy};
use pierdesk::clipboard::{
    ClipboardProbeOptions, DragProbeEdge, FileCaptureProbeOptions, LoopbackProbeOptions,
    PauseProbeOptions, RemoteTargetDragProbeOptions, SecureFetchProbeOptions,
    SecureReceiverProbeOptions, SecureSourceProbeOptions, SecureSourceTls, SourceDragProbeOptions,
    TargetDragProbeOptions, run_clipboard_probe, run_drag_edge_helper, run_drag_helper_self_test,
    run_file_capture_probe, run_loopback_probe, run_pause_probe, run_remote_target_drag_probe,
    run_secure_fetch_probe, run_secure_receiver_probe, run_secure_source_probe,
    run_source_drag_probe, run_target_drag_probe,
};
use pierdesk::cross_screen::{
    AutomaticChannelDiagnosticOptions, ChannelDiagnosticOptions, ChannelDiagnosticRole,
    InputDiagnosticOptions, LiveControlDiagnosticOptions, run_automatic_channel_diagnostic,
    run_boundary_diagnostic, run_channel_diagnostic, run_control_protocol_diagnostic,
    run_input_diagnostic, run_live_control_diagnostic,
};
use pierdesk::device_store::DeviceStore;
use pierdesk::pairing::{PendingPairing, connect_for_pairing, listen_for_pairing};
use pierdesk::security::{
    CertificateFingerprint, PinnedTlsClient, PinnedTlsServer, TlsIdentity, TrustedTlsServer,
    generate_test_identity, load_and_verify_peer_certificate,
};

fn main() -> ExitCode {
    // GUI startup never allocates a console. Explicit CLI commands may attach to an
    // existing terminal; inherited pipe handles (SSH/askpass/tests) stay untouched.
    if std::env::args_os().len() > 1
        && !matches!(
            std::env::args().nth(1).as_deref(),
            Some(
                "tray"
                    | "tray-replace"
                    | "update-apply"
                    | "file-drag-edge-helper"
                    | "file-drag-edge-launcher"
                    | "screenshot-helper"
                    | "screenshot-warm-helper"
                    | "ssh-proxy"
            )
        )
    {
        pierdesk::update_support::attach_cli_console();
    }
    // ProxyCommand inherits SSH_ASKPASS variables. Explicit proxy mode must win, otherwise the
    // askpass handler could emit a key passphrase into the SSH protocol byte stream.
    if std::env::args().nth(1).as_deref() == Some("ssh-proxy") {
        return match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Pierdesk SSH: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if std::env::var_os(pierdesk::ssh::KEYGEN_SECRET_ENV).is_some() {
        return match pierdesk::ssh::run_keygen_askpass() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    if std::env::var_os("PIERDESK_SSH_ASKPASS_ID").is_some() {
        return match pierdesk::ssh::run_askpass() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    if std::env::args_os().len() == 1 {
        return run_tray(true);
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("Pierdesk error: {message}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        println!("{}", pierdesk::validation_banner());
        print_usage();
        return Ok(());
    };

    if command == "ssh-proxy" {
        let id = arguments.next().ok_or("Missing SSH connection identity")?;
        reject_remaining_arguments(&mut arguments, &command)?;
        return pierdesk::ssh::run_proxy(&id).map_err(|e| e.to_string());
    }
    if command == "ssh-access-status" || command == "ssh-access-upgrade" {
        let status = if command == "ssh-access-upgrade" {
            if arguments.next().as_deref() != Some("--confirm-disconnect") {
                return Err("Upgrading restarts SSH and disconnects sessions. Use --confirm-disconnect after reviewing this consequence.".into());
            }
            reject_remaining_arguments(&mut arguments, &command)?;
            pierdesk::ssh::upgrade_access()
        } else {
            reject_remaining_arguments(&mut arguments, &command)?;
            pierdesk::ssh::access_status()
        };
        println!("{}", status.map_err(|e| e.to_string())?);
        return Ok(());
    }
    if command == "update-apply" {
        return pierdesk::update_support::run_apply(arguments.collect());
    }
    if command == "screenshot-helper" {
        let language = arguments.next().unwrap_or_else(|| "en".into());
        reject_remaining_arguments(&mut arguments, &command)?;
        if let Err(error) = pierdesk::screenshot::run(&language) {
            use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
            let message: Vec<u16> = format!("截图未完成 / Screenshot failed\n{error}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                MessageBoxW(
                    None,
                    windows_core::PCWSTR(message.as_ptr()),
                    windows_core::w!("Pierdesk"),
                    MB_OK | MB_ICONERROR,
                );
            }
            return Err(error.to_string());
        }
        return Ok(());
    }
    if command == "screenshot-warm-helper" {
        let parent_id = arguments
            .next()
            .ok_or("Missing parent process ID")?
            .parse::<u32>()
            .map_err(|_| "Invalid parent process ID")?;
        reject_remaining_arguments(&mut arguments, &command)?;
        return pierdesk::screenshot::warm(parent_id).map_err(|error| error.to_string());
    }
    if matches!(command.as_str(), "version" | "--version") {
        reject_remaining_arguments(&mut arguments, &command)?;
        println!("Pierdesk {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    #[cfg(feature = "licensing")]
    if command.starts_with("license-app-") {
        reject_remaining_arguments(&mut arguments, &command)?;
        let client = pierdesk::license_runtime::client().map_err(|e| e.to_string())?;
        let endpoint = pierdesk::license_runtime::ENDPOINT;
        match command.as_str() {
            "license-app-device" => {
                println!("{}", client.device_public_key().map_err(|e| e.to_string())?);
            }
            "license-app-status" => {
                pierdesk::license_runtime::require_access().map_err(|e| e.to_string())?;
                println!("active");
            }
            "license-app-activate" => {
                let mut code = zeroize::Zeroizing::new(String::new());
                std::io::stdin()
                    .take(256)
                    .read_to_string(&mut code)
                    .map_err(|e| e.to_string())?;
                client
                    .activate(endpoint, code.trim())
                    .map_err(|e| e.to_string())?;
                println!("activated");
            }
            "license-app-refresh" => {
                client.refresh(endpoint).map_err(|e| e.to_string())?;
                println!("verified");
            }
            "license-app-release" => {
                client.release(endpoint).map_err(|e| e.to_string())?;
                println!("released");
            }
            _ => return Err("unknown license command".into()),
        }
        return Ok(());
    }
    if !command.starts_with("license-")
        && !matches!(
            command.as_str(),
            "tray"
                | "tray-replace"
                | "tray-self-test"
                | "ssh-keygen-self-test"
                | "tray-show"
                | "tray-exit"
                | "help"
                | "--help"
                | "version"
                | "--version"
                | "about"
                | "ferry-ticket"
                | "settings-wizard"
                | "settings-show"
                | "pair-wizard"
                | "trust-wizard"
                | "autostart-status"
                | "autostart-enable"
                | "autostart-disable"
        )
    {
        pierdesk::license_runtime::require_access().map_err(|e| e.to_string())?;
    }
    match command.as_str() {
        "tray" => {
            let detach_console = match arguments.next().as_deref() {
                None => true,
                Some("--console") if arguments.next().is_none() => false,
                Some(argument) => return Err(format!("unknown tray argument: {argument}")),
            };
            pierdesk::tray::run(detach_console)
        }
        "tray-replace" => {
            let process_id = arguments
                .next()
                .ok_or_else(|| "tray-replace requires a process id".to_owned())?
                .parse::<u32>()
                .map_err(|error| format!("invalid tray-replace process id: {error}"))?;
            if let Some(argument) = arguments.next() {
                return Err(format!("unknown tray-replace argument: {argument}"));
            }
            pierdesk::tray::run_after_process(process_id)
        }
        "tray-self-test" => pierdesk::tray::self_test(),
        "ssh-keygen-self-test" => {
            reject_remaining_arguments(&mut arguments, "ssh-keygen-self-test")?;
            let askpass = std::env::current_exe().map_err(|error| error.to_string())?;
            pierdesk::ssh::keygen_askpass_self_test(&askpass)
                .map_err(|error| error.to_string())?;
            println!("ssh-keygen self-test passed");
            Ok(())
        }
        "tray-show" => pierdesk::tray::show_existing_status(),
        "tray-exit" => pierdesk::tray::exit_existing(),
        "shared-text-capture-helper" => {
            reject_remaining_arguments(&mut arguments, "shared-text-capture-helper")?;
            pierdesk::clipboard::run_shared_clipboard_capture_helper()
                .map_err(|error| format!("shared text capture helper failed: {error}"))
        }
        "file-drag-edge-helper" => {
            let (port, token, edge, thickness) = parse_drag_edge_helper_options(arguments)?;
            run_drag_edge_helper(port, token, edge, thickness)
                .map_err(|error| format!("file drag edge helper failed: {error}"))
        }
        "file-drag-edge-launcher" => {
            let (port, token, edge, thickness) = parse_drag_edge_helper_options(arguments)?;
            pierdesk::clipboard::launch_drag_edge_helper(port, &token, edge, thickness)
                .map_err(|error| format!("file drag edge launch failed: {error}"))
        }
        "file-drag-helper-self-test" => {
            reject_remaining_arguments(&mut arguments, "file-drag-helper-self-test")?;
            let store = DeviceStore::current_user()
                .map_err(|error| format!("could not resolve Pierdesk data directory: {error}"))?;
            let (process_id, elevated, bounds) = run_drag_helper_self_test(store.root())
                .map_err(|error| format!("file drag helper self-test failed: {error}"))?;
            println!(
                "FILE_DRAG_HELPER status=ok process_id={process_id} elevated={elevated} window={},{},{},{}",
                bounds[0], bounds[1], bounds[2], bounds[3]
            );
            Ok(())
        }
        "about" => {
            reject_remaining_arguments(&mut arguments, "about")?;
            let text = if wizard_english() {
                pierdesk::about::dialog_text_english()
            } else {
                pierdesk::about::dialog_text()
            };
            println!("{text}");
            Ok(())
        }
        "ferry-ticket" => {
            reject_remaining_arguments(&mut arguments, "ferry-ticket")?;
            print!("{}", pierdesk::about::ferry_ticket());
            Ok(())
        }
        "autostart-status" => {
            println!("AUTOSTART state={}", pierdesk::tray::autostart_status()?);
            Ok(())
        }
        "autostart-enable" => {
            pierdesk::tray::set_autostart(true)?;
            println!("AUTOSTART state=enabled");
            Ok(())
        }
        "autostart-disable" => {
            pierdesk::tray::set_autostart(false)?;
            println!("AUTOSTART state=disabled");
            Ok(())
        }
        "pair-wizard" => run_pair_wizard(),
        "trust-wizard" => run_trust_wizard(),
        "settings-wizard" => run_settings_wizard(),
        "settings-show" => run_settings_show(),
        #[cfg(feature = "licensing")]
        "license-device" | "license-activate" | "license-refresh" | "license-status"
        | "license-release" => run_license_command(&command, &mut arguments),
        "clipboard-test" => {
            let mut lifetime = None;
            while let Some(argument) = arguments.next() {
                if argument != "--lifetime-seconds" {
                    return Err(format!("unknown argument: {argument}"));
                }
                let seconds = arguments
                    .next()
                    .ok_or_else(|| "--lifetime-seconds requires a value".to_owned())?
                    .parse::<u64>()
                    .map_err(|error| format!("invalid lifetime: {error}"))?;
                lifetime = Some(Duration::from_secs(seconds));
            }
            run_clipboard_probe(ClipboardProbeOptions { lifetime })
                .map_err(|error| format!("{error} ({:#010X})", error.code().0.cast_unsigned()))
        }
        "clipboard-pause-test" => {
            let options = parse_pause_probe_options(arguments)?;
            run_pause_probe(options)
                .map_err(|error| format!("{error} ({:#010X})", error.code().0.cast_unsigned()))
        }
        "loopback-clipboard-test" => {
            let options = parse_loopback_probe_options(arguments)?;
            run_loopback_probe(options)
                .map_err(|error| format!("{error} ({:#010X})", error.code().0.cast_unsigned()))
        }
        "file-capture-test" => {
            let options = parse_file_capture_probe_options(arguments)?;
            run_file_capture_probe(options)
                .map_err(|error| format!("{error} ({:#010X})", error.code().0.cast_unsigned()))
        }
        "file-drag-source-probe" => {
            let options = parse_source_drag_probe_options(arguments)?;
            let report = run_source_drag_probe(options)
                .map_err(|error| format!("file drag source probe failed: {error}"))?;
            println!(
                "FILE_DRAG_SOURCE_PROBE completed=true event={} hdrop={} file_descriptor={} items={} absolute_paths={} held_readable={} clipboard_unchanged={}",
                report.event,
                report.hdrop_available,
                report.file_descriptor_available,
                report.item_count,
                report.all_paths_absolute,
                report.held_object_still_readable,
                report.clipboard_unchanged,
            );
            Ok(())
        }
        "file-drag-target-probe" => {
            let options = parse_target_drag_probe_options(arguments)?;
            let report = run_target_drag_probe(&options)
                .map_err(|error| format!("file drag target probe failed: {error}"))?;
            println!(
                "FILE_DRAG_TARGET_PROBE completed=true result={} effect={} drag_image={} private_format_writes={} async_finished={} read_calls={} bytes_read={} clipboard_unchanged={}",
                report.result,
                report.effect,
                report.drag_image_initialized,
                report.private_format_writes,
                report.async_operation_finished,
                report.content_read_calls,
                report.content_bytes_read,
                report.clipboard_unchanged,
            );
            Ok(())
        }
        "file-drag-remote-target-probe" => {
            let parsed = parse_secure_client_options(arguments, false, true)?;
            let client = build_secure_client(&parsed)?;
            let report = run_remote_target_drag_probe(&RemoteTargetDragProbeOptions {
                client,
                lifetime: parsed.lifetime.unwrap_or(Duration::from_mins(2)),
                auto_cancel: parsed.auto_cancel,
            })
            .map_err(|error| format!("remote target drag probe failed: {error}"))?;
            println!(
                "FILE_DRAG_REMOTE_TARGET_PROBE completed=true result={} effect={} drag_image={} private_format_writes={} async_finished={} read_calls={} bytes_read={} clipboard_unchanged={}",
                report.result,
                report.effect,
                report.drag_image_initialized,
                report.private_format_writes,
                report.async_operation_finished,
                report.content_read_calls,
                report.content_bytes_read,
                report.clipboard_unchanged,
            );
            Ok(())
        }
        "cross-screen-boundary-test" => {
            if let Some(argument) = arguments.next() {
                return Err(format!(
                    "cross-screen-boundary-test takes no arguments: {argument}"
                ));
            }
            let report = run_boundary_diagnostic()
                .map_err(|error| format!("cross-screen boundary diagnostic failed: {error}"))?;
            println!(
                "CROSS_SCREEN_BOUNDARY_TEST completed=true clean={} layouts={} dpi_profiles={} resolution_pairs={} transition_round_trips={} rejected_wrong_edges={} blocked_corner_samples={} max_mapping_error_pixels={}",
                report.clean,
                report.layouts,
                report.dpi_profiles,
                report.resolution_pairs,
                report.transition_round_trips,
                report.rejected_wrong_edges,
                report.blocked_corner_samples,
                report.maximum_mapping_error_pixels,
            );
            if report.clean {
                Ok(())
            } else {
                Err("cross-screen boundary diagnostic was not clean".to_owned())
            }
        }
        "cross-screen-control-test" => {
            if let Some(argument) = arguments.next() {
                return Err(format!(
                    "cross-screen-control-test takes no arguments: {argument}"
                ));
            }
            println!(
                "CROSS_SCREEN_CONTROL_TEST started=true transport=loopback_tls input_suppressed=false remote_injection=false"
            );
            let report = run_control_protocol_diagnostic()
                .map_err(|error| format!("cross-screen control diagnostic failed: {error}"))?;
            println!(
                "CROSS_SCREEN_CONTROL_TEST completed=true clean={} screen_info={} enter_round_trips={} return_round_trips={} input_events={} rejected_input_epochs={} rejected_wrong_boundaries={} rejected_replays={} rejected_stale_acks={} local_release_requests={} control_epoch={} first_channel_epoch={} second_channel_epoch={}",
                report.clean,
                report.screen_info_exchanges,
                report.enter_round_trips,
                report.return_round_trips,
                report.input_events_round_tripped,
                report.rejected_input_epochs,
                report.rejected_wrong_boundaries,
                report.rejected_replays,
                report.rejected_stale_acknowledgements,
                report.local_release_requests,
                report.control_epoch,
                report.first_channel_epoch,
                report.second_channel_epoch,
            );
            if report.clean {
                Ok(())
            } else {
                Err("cross-screen control diagnostic was not clean".to_owned())
            }
        }
        "cross-screen-input-test" => {
            let options = parse_cross_screen_input_options(arguments)?;
            println!(
                "CROSS_SCREEN_INPUT_TEST started=true lifetime_ms={} queue_capacity=256 emergency=Ctrl+Alt+Shift+Esc",
                options.lifetime.as_millis()
            );
            let report = run_input_diagnostic(options)
                .map_err(|error| format!("cross-screen input diagnostic failed: {error}"))?;
            println!(
                "CROSS_SCREEN_INPUT_TEST completed=true stop={} clean={} dpi_mode={} effective_dpi={} initial_metrics={}x{} primary_physical={}x{} captured={} injected={} key_down={} key_repeat={} key_up={} mouse_down={} mouse_repeat={} mouse_up={} mouse_move={} wheel={} untracked_release={} emergency_return={} forced_capture_release={} forced_injected_release={} captured_remaining={} injected_keys_remaining={} injected_buttons_remaining={} queue_peak={} move_coalesced={} move_bypassed={} move_stale={}",
                report.stop_reason.label(),
                report.clean_shutdown,
                report.dpi_mode.label(),
                report.effective_dpi,
                report.initial_metrics_width,
                report.initial_metrics_height,
                report.primary_physical_width,
                report.primary_physical_height,
                report.captured.total_observed(),
                report.injected_events,
                report.captured.key_downs,
                report.captured.key_repeats,
                report.captured.key_ups,
                report.captured.mouse_button_downs,
                report.captured.mouse_button_repeats,
                report.captured.mouse_button_ups,
                report.captured.mouse_moves,
                report.captured.wheel_events,
                report.captured.untracked_releases,
                report.captured.emergency_returns,
                report.forced_capture_releases,
                report.forced_injected_releases,
                report.remaining_captured_inputs,
                report.remaining_injected_keys,
                report.remaining_injected_buttons,
                report.queue_peak_depth,
                report.coalesced_mouse_moves,
                report.bypassed_mouse_moves,
                report.stale_mouse_moves,
            );
            if report.clean_shutdown {
                Ok(())
            } else {
                Err(report.failure.unwrap_or_else(|| {
                    "cross-screen input diagnostic did not release all input".to_owned()
                }))
            }
        }
        "cross-screen-channel-test" => {
            let options = parse_cross_screen_channel_options(arguments)?;
            println!(
                "CROSS_SCREEN_CHANNEL_TEST started=true role={} address={} peer={} lifetime_ms={} input_suppressed=false remote_injection=false",
                options.role.role().label(),
                options.role.address(),
                options.peer_fingerprint,
                options.lifetime.as_millis()
            );
            let report = run_channel_diagnostic(&options)
                .map_err(|error| format!("cross-screen channel diagnostic failed: {error}"))?;
            let snapshot = report.snapshot;
            let round_trip = snapshot
                .round_trip_micros
                .map_or_else(|| "none".to_owned(), |micros| micros.to_string());
            println!(
                "CROSS_SCREEN_CHANNEL_TEST completed=true state={} clean={} role={} local={} peer_address={} local_fingerprint={} peer_fingerprint={} session={} channel_epoch={} sampled_mouse={} sent_frames={} received_frames={} sent_mouse={} received_mouse={} move_coalesced={} sent_heartbeat={} received_heartbeat={} rtt_us={} sent_release_all={} received_release_all={} last_sent_sequence={} last_received_sequence={}",
                snapshot.state.label(),
                snapshot.clean_shutdown,
                snapshot.role.label(),
                snapshot.local_address,
                snapshot.peer_address,
                snapshot.local_fingerprint,
                snapshot.peer_fingerprint,
                snapshot.session_id,
                snapshot.channel_epoch,
                report.sampled_mouse_positions,
                snapshot.sent_frames,
                snapshot.received_frames,
                snapshot.sent_mouse_moves,
                snapshot.received_mouse_moves,
                snapshot.coalesced_mouse_moves,
                snapshot.sent_heartbeats,
                snapshot.received_heartbeats,
                round_trip,
                snapshot.sent_release_all,
                snapshot.received_release_all,
                snapshot.last_sent_sequence,
                snapshot.last_received_sequence,
            );
            if snapshot.clean_shutdown {
                Ok(())
            } else {
                Err(snapshot
                    .last_error
                    .unwrap_or_else(|| "cross-screen channel did not stop cleanly".to_owned()))
            }
        }
        "cross-screen-auto-test" => {
            let options = parse_cross_screen_automatic_options(arguments)?;
            println!(
                "CROSS_SCREEN_AUTO_TEST started=true address_source=paired_settings identity_source=trusted_store lifetime_ms={} connect_timeout_ms={} input_suppressed=false remote_injection=false",
                options.lifetime.as_millis(),
                options.connect_timeout.as_millis(),
            );
            let report = run_automatic_channel_diagnostic(&options).map_err(|error| {
                format!("automatic cross-screen channel diagnostic failed: {error}")
            })?;
            let snapshot = report.snapshot;
            let channel = snapshot.channel.clone().ok_or_else(|| {
                "automatic cross-screen runtime stopped without a channel snapshot".to_owned()
            })?;
            let round_trip = channel
                .round_trip_micros
                .map_or_else(|| "none".to_owned(), |micros| micros.to_string());
            let local_screen = snapshot.control.local_screen.map_or_else(
                || "none".to_owned(),
                |screen| format!("{}x{}", screen.width, screen.height),
            );
            let peer_screen = snapshot.control.peer_screen.map_or_else(
                || "none".to_owned(),
                |screen| format!("{}x{}", screen.width, screen.height),
            );
            let clean = snapshot.clean_shutdown && channel.clean_shutdown;
            println!(
                "CROSS_SCREEN_AUTO_TEST completed=true state={} clean={} role={} local={} peer_address={} local_fingerprint={} peer_fingerprint={} attempts={} connections={} reconnects={} session={} channel_epoch={} control_ready={} control_state={} local_screen={} peer_screen={} control_handshakes={} screen_info_sent={} screen_info_received={} rejected_transitions={} forced_local_resets={} sampled_mouse={} queued_mouse={} runtime_move_coalesced={} sent_frames={} received_frames={} sent_mouse={} received_mouse={} channel_move_coalesced={} sent_heartbeat={} received_heartbeat={} rtt_us={} sent_release_all={} received_release_all={}",
                snapshot.state.label(),
                clean,
                snapshot.role.label(),
                snapshot.local_endpoint,
                snapshot.peer_endpoint,
                snapshot.local_fingerprint,
                snapshot.peer_fingerprint,
                snapshot.connection_attempts,
                snapshot.successful_connections,
                snapshot.reconnects,
                channel.session_id,
                channel.channel_epoch,
                snapshot.control.ready,
                snapshot.control.state.label(),
                local_screen,
                peer_screen,
                snapshot.control.successful_handshakes,
                snapshot.control.screen_info_sent,
                snapshot.control.screen_info_received,
                snapshot.control.rejected_transition_requests,
                snapshot.control.forced_local_resets,
                report.sampled_mouse_positions,
                snapshot.queued_mouse_positions,
                snapshot.runtime_coalesced_mouse_positions,
                channel.sent_frames,
                channel.received_frames,
                channel.sent_mouse_moves,
                channel.received_mouse_moves,
                channel.coalesced_mouse_moves,
                channel.sent_heartbeats,
                channel.received_heartbeats,
                round_trip,
                channel.sent_release_all,
                channel.received_release_all,
            );
            if clean {
                Ok(())
            } else {
                Err(snapshot
                    .last_error
                    .or(channel.last_error)
                    .unwrap_or_else(|| {
                        "automatic cross-screen channel did not stop cleanly".to_owned()
                    }))
            }
        }
        "cross-screen-live-test" => {
            let automatic = parse_cross_screen_automatic_options(arguments)?;
            let options = LiveControlDiagnosticOptions {
                store: automatic.store,
                connect_timeout: automatic.connect_timeout,
                lifetime: automatic.lifetime,
            };
            println!(
                "CROSS_SCREEN_LIVE_TEST started=true layout=automatic_role_pair corner_guard_pixels=32 emergency=Ctrl+Alt+Shift+Esc input_suppressed_after_ack=true remote_injection=true lifetime_ms={} connect_timeout_ms={}",
                options.lifetime.as_millis(),
                options.connect_timeout.as_millis(),
            );
            let report = run_live_control_diagnostic(&options)
                .map_err(|error| format!("live cross-screen control diagnostic failed: {error}"))?;
            println!(
                "CROSS_SCREEN_LIVE_TEST completed=true clean={} stop={} role={} local_screen={}x{} peer_screen={}x{} control_entries={} control_returns={} enter_latency_samples={} enter_latency_avg_us={} enter_latency_max_us={} return_latency_samples={} return_latency_avg_us={} return_latency_max_us={} rejected_input_epochs={} ignored_stale_acks={} forwarded_keys_down={} forwarded_key_repeats={} forwarded_keys_up={} forwarded_buttons_down={} forwarded_buttons_up={} forwarded_moves={} forwarded_wheel={} injected_events={} forced_injected_releases={} forwarded_remaining={} injected_remaining={} queue_peak={} queue_coalesced_moves={} queue_bypassed_moves={} session={} channel_epoch={} sent_input={} received_input={} sent_release_all={} received_release_all={} channel_state={} failure={}",
                report.clean_shutdown,
                report.stop_reason.label(),
                report.role.label(),
                report.local_screen.width,
                report.local_screen.height,
                report.peer_screen.width,
                report.peer_screen.height,
                report.control_entries,
                report.control_returns,
                report.enter_latency_samples,
                report.enter_latency_average_micros,
                report.enter_latency_max_micros,
                report.return_latency_samples,
                report.return_latency_average_micros,
                report.return_latency_max_micros,
                report.rejected_input_epochs,
                report.ignored_stale_acknowledgements,
                report.forwarded.key_downs,
                report.forwarded.key_repeats,
                report.forwarded.key_ups,
                report.forwarded.mouse_button_downs,
                report.forwarded.mouse_button_ups,
                report.forwarded.mouse_moves,
                report.forwarded.wheel_events,
                report.injected_events,
                report.forced_injected_releases,
                report.remaining_forwarded_inputs,
                report.remaining_injected_inputs,
                report.queue_peak_depth,
                report.queue_coalesced_moves,
                report.queue_bypassed_moves,
                report.channel.session_id,
                report.channel.channel_epoch,
                report.channel.sent_input_events,
                report.channel.received_input_events,
                report.channel.sent_release_all,
                report.channel.received_release_all,
                report.channel.state.label(),
                report.failure.as_deref().unwrap_or("none"),
            );
            if report.clean_shutdown {
                Ok(())
            } else {
                Err(report
                    .failure
                    .unwrap_or_else(|| "live cross-screen control did not stop cleanly".to_owned()))
            }
        }
        "pair-listen" => {
            let options = parse_pairing_options(arguments, "--listen")?;
            let pending = listen_for_pairing(options.store, options.address, options.timeout)
                .map_err(|error| format!("pairing listener failed: {error}"))?;
            confirm_pairing(pending, &options.label)
        }
        "pair-connect" => {
            let options = parse_pairing_options(arguments, "--connect")?;
            let pending = connect_for_pairing(options.store, options.address, options.timeout)
                .map_err(|error| format!("pairing connection failed: {error}"))?;
            confirm_pairing(pending, &options.label)
        }
        "device-init" => {
            let store = parse_store_only(arguments)?;
            let stored = store
                .load_or_create_identity()
                .map_err(|error| format!("device identity initialization failed: {error}"))?;
            println!(
                "DEVICE created={} fingerprint={} store={}",
                stored.created,
                stored.identity.fingerprint(),
                store.root().display()
            );
            Ok(())
        }
        "device-show" => {
            let store = parse_store_only(arguments)?;
            let identity = store
                .load_identity()
                .map_err(|error| format!("device identity load failed: {error}"))?;
            println!(
                "DEVICE fingerprint={} store={}",
                identity.fingerprint(),
                store.root().display()
            );
            Ok(())
        }
        "device-export-cert" => {
            let (store, output) = parse_store_and_output(arguments)?;
            let fingerprint = store
                .export_certificate(&output)
                .map_err(|error| format!("certificate export failed: {error}"))?;
            println!(
                "CERTIFICATE exported=true fingerprint={fingerprint} path={}",
                output.display()
            );
            Ok(())
        }
        "trust-import" => {
            let (store, certificate, fingerprint, label) = parse_trust_import(arguments)?;
            let peer = store
                .trust_peer_file(&certificate, fingerprint, &label)
                .map_err(|error| format!("trust import failed: {error}"))?;
            println!(
                "TRUST imported=true fingerprint={} label={:?}",
                peer.fingerprint, peer.label
            );
            Ok(())
        }
        "trust-list" => {
            let store = parse_store_only(arguments)?;
            let peers = store
                .list_peers()
                .map_err(|error| format!("trust list failed: {error}"))?;
            println!("TRUST peers={}", peers.len());
            for peer in peers {
                println!(
                    "PEER fingerprint={} label={:?}",
                    peer.fingerprint, peer.label
                );
            }
            Ok(())
        }
        "trust-revoke" => {
            let (store, fingerprint) = parse_store_and_fingerprint(arguments)?;
            store
                .revoke_peer(fingerprint)
                .map_err(|error| format!("trust revocation failed: {error}"))?;
            println!("TRUST revoked=true fingerprint={fingerprint}");
            Ok(())
        }
        "identity-test-generate" => {
            let (certificate, private_key) = parse_identity_generation_options(arguments)?;
            let fingerprint = generate_test_identity(&certificate, &private_key)
                .map_err(|error| format!("identity generation failed: {error}"))?;
            println!(
                "IDENTITY generated=true certificate={} private_key={} fingerprint={fingerprint}",
                certificate.display(),
                private_key.display()
            );
            Ok(())
        }
        "secure-source-test" => {
            let parsed = parse_secure_source_options(arguments)?;
            let (tls, local_fingerprint, peer_fingerprint) =
                parsed.tls.load_source(parsed.io_timeout)?;
            println!(
                "IDENTITY local_fingerprint={local_fingerprint} pinned_peer={peer_fingerprint}"
            );
            run_secure_source_probe(SecureSourceProbeOptions {
                listen_address: parsed.listen_address,
                source_paths: parsed.source_paths,
                offer_ttl: parsed.offer_ttl,
                transfer_ttl: parsed.transfer_ttl,
                io_timeout: parsed.io_timeout,
                lifetime: parsed.lifetime,
                tls,
            })
            .map_err(|error| format!("secure source failed: {error}"))
        }
        "secure-fetch-test" => {
            let parsed = parse_secure_client_options(arguments, true, false)?;
            let client = build_secure_client(&parsed)?;
            let output_path = parsed
                .output_path
                .ok_or_else(|| "--output is required".to_owned())?;
            let result = run_secure_fetch_probe(SecureFetchProbeOptions {
                client,
                output_path,
            })
            .map_err(|error| format!("secure fetch failed: {error}"))?;
            println!(
                "FETCH completed=true bytes={} sha256={} state={:?}",
                result.bytes,
                encode_hex(&result.sha256),
                result.status.state
            );
            Ok(())
        }
        "secure-receiver-test" => {
            let parsed = parse_secure_client_options(arguments, false, false)?;
            let client = build_secure_client(&parsed)?;
            run_secure_receiver_probe(&SecureReceiverProbeOptions {
                client,
                lifetime: parsed.lifetime,
                async_mode: parsed.async_mode,
            })
            .map_err(|error| format!("{error} ({:#010X})", error.code().0.cast_unsigned()))
        }
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        _ => Err(format!("unknown command: {command}")),
    }
}

fn print_usage() {
    println!("Usage:");
    println!("  pierdesk tray [--console]");
    println!("  pierdesk tray-self-test|tray-show|tray-exit");
    println!("  pierdesk ssh-keygen-self-test");
    println!("  pierdesk about");
    println!("  pierdesk autostart-status|autostart-enable|autostart-disable");
    println!("  pierdesk pair-wizard|trust-wizard|settings-wizard|settings-show");
    #[cfg(feature = "licensing")]
    {
        println!(
            "  pierdesk license-app-device|license-app-status|license-app-activate|license-app-refresh|license-app-release"
        );
        println!("  pierdesk license-device|license-status <store-dir> <issuer-public-key>");
        println!(
            "  pierdesk license-activate|license-refresh <store-dir> <issuer-public-key> <server-url>"
        );
    }
    println!("  pierdesk clipboard-test [--lifetime-seconds <seconds>]");
    println!(
        "  pierdesk clipboard-pause-test [--size-mib <MiB>] [--chunk-kib <KiB>] [--delay-ms <ms>] [--async-mode] [--lifetime-seconds <seconds>]"
    );
    println!(
        "  pierdesk loopback-clipboard-test [--size-mib <MiB>] [--range-kib <KiB>] [--fragment-bytes <bytes>] [--delay-ms <ms>] [--io-timeout-seconds <seconds>] [--async-mode] [--lifetime-seconds <seconds>]"
    );
    println!(
        "  pierdesk file-capture-test [--offer-ttl-seconds <seconds>] [--async-mode] [--lifetime-seconds <seconds>]"
    );
    println!(
        "  pierdesk file-drag-source-probe --edge <left|right|top|bottom> [--thickness <1..64>] [--lifetime-seconds <1..300>] [--settle-ms <1..5000>]"
    );
    println!(
        "  pierdesk file-drag-target-probe --file <absolute-path> [--lifetime-seconds <1..300>] [--auto-cancel]"
    );
    println!(
        "  pierdesk file-drag-remote-target-probe --connect <private-ip:port> --peer-fingerprint <sha256> (--store <directory> | --identity-cert <DER> --identity-key <DER> --peer-cert <DER>) [--lifetime-seconds <seconds>] [--auto-cancel]"
    );
    println!("  pierdesk cross-screen-boundary-test");
    println!("  pierdesk cross-screen-control-test");
    println!("  pierdesk cross-screen-input-test [--lifetime-seconds <1..300>]");
    println!(
        "  pierdesk cross-screen-channel-test (--listen <private-ip:port> | --connect <private-ip:port>) --peer-fingerprint <SHA-256> [--store <directory>] [--connect-timeout-seconds <1..300>] [--lifetime-seconds <1..300>]"
    );
    println!(
        "  pierdesk cross-screen-auto-test [--store <directory>] [--connect-timeout-seconds <1..300>] [--lifetime-seconds <1..300>]"
    );
    println!(
        "  pierdesk cross-screen-live-test [--store <directory>] [--connect-timeout-seconds <1..300>] [--lifetime-seconds <1..300>]"
    );
    println!(
        "  pierdesk pair-listen --listen <private-ip:port> --label <peer-name> [--store <directory>] [--timeout-seconds <1..300>]"
    );
    println!(
        "  pierdesk pair-connect --connect <private-ip:port> --label <peer-name> [--store <directory>] [--timeout-seconds <1..300>]"
    );
    println!("  pierdesk device-init [--store <directory>]");
    println!("  pierdesk device-show [--store <directory>]");
    println!("  pierdesk device-export-cert --out <certificate.der> [--store <directory>]");
    println!(
        "  pierdesk trust-import --cert <certificate.der> --fingerprint <SHA-256> --label <name> [--store <directory>]"
    );
    println!("  pierdesk trust-list [--store <directory>]");
    println!("  pierdesk trust-revoke --fingerprint <SHA-256> [--store <directory>]");
    println!(
        "  pierdesk identity-test-generate --cert-out <certificate.der> --key-out <private-key.der>"
    );
    println!(
        "  pierdesk secure-source-test --listen <private-ip:port> --file <file-or-directory> [--file <additional-path> ...] --identity-cert <certificate.der> --identity-key <private-key.der> --peer-cert <certificate.der> --peer-fingerprint <SHA-256> [--offer-ttl-seconds <seconds>] [--transfer-ttl-seconds <seconds>] [--io-timeout-seconds <seconds>] [--lifetime-seconds <seconds>]"
    );
    println!(
        "  pierdesk secure-fetch-test --connect <private-ip:port> --output <new-path> --identity-cert <certificate.der> --identity-key <private-key.der> --peer-cert <certificate.der> --peer-fingerprint <SHA-256> [--io-timeout-seconds <seconds>] [--recovery-seconds <0..3600>]"
    );
    println!(
        "  pierdesk secure-receiver-test --connect <private-ip:port> --identity-cert <certificate.der> --identity-key <private-key.der> --peer-cert <certificate.der> --peer-fingerprint <SHA-256> [--io-timeout-seconds <seconds>] [--recovery-seconds <0..3600>] [--async-mode] [--lifetime-seconds <seconds>]"
    );
    println!(
        "  For secure-*-test, --store <directory> --peer-fingerprint <SHA-256> replaces the three DER path options"
    );
}

fn reject_remaining_arguments(
    arguments: &mut impl Iterator<Item = String>,
    command: &str,
) -> Result<(), String> {
    if let Some(argument) = arguments.next() {
        Err(format!("{command} takes no arguments: {argument}"))
    } else {
        Ok(())
    }
}

fn run_tray(detach_console: bool) -> ExitCode {
    match pierdesk::tray::run(detach_console) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("Pierdesk error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Language of the console wizards: follows the interface language in Settings (English by default).
fn wizard_english() -> bool {
    static ENGLISH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENGLISH.get_or_init(|| {
        DeviceStore::current_user()
            .ok()
            .and_then(|store| pierdesk::app_settings::UiPreferences::load_or_default(&store).ok())
            .is_none_or(|preferences| preferences.language == pierdesk::app_settings::UiLanguage::English)
    })
}

/// Chooses the Chinese or English text of a console message.
macro_rules! tr {
    ($zh:expr, $en:expr) => {
        if wizard_english() { $en } else { $zh }
    };
}

fn run_pair_wizard() -> Result<(), String> {
    let result = run_pair_wizard_inner();
    if let Err(error) = &result {
        eprintln!("\n{}{error}", tr!("配对失败：", "Pairing failed: "));
    }
    wait_for_enter(tr!("\n按 Enter 关闭此窗口...", "\nPress Enter to close this window..."))?;
    result
}

fn run_pair_wizard_inner() -> Result<(), String> {
    let store = DeviceStore::current_user()
        .map_err(|error| format!("device store unavailable: {error}"))?;
    let stored = store
        .load_or_create_identity()
        .map_err(|error| format!("device identity initialization failed: {error}"))?;
    println!("{}", tr!("Pierdesk 设备配对", "Pierdesk device pairing"));
    println!("{}{}", tr!("本机指纹：", "Local fingerprint: "), stored.identity.fingerprint());
    println!("{}", tr!("\n两台电脑分别打开此向导：一端选择 1，另一端选择 2。", "\nOpen this wizard on both computers: choose 1 on one and 2 on the other."));
    println!("{}", tr!("核对两端出现的 verify_code 完全一致后，必须输入大写 YES。\n", "Check that the verify_code on both computers is identical, then type YES in capitals.\n"));
    println!("{}", tr!("[1] 监听配对", "[1] Listen for pairing"));
    println!("{}", tr!("[2] 连接配对", "[2] Connect to pair"));
    let role = prompt(tr!("请选择 1 或 2: ", "Choose 1 or 2: "))?;
    let address_text = match role.as_str() {
        "1" => prompt(tr!("本机监听地址（例如 192.168.1.29:45232）: ", "Address to listen on (for example 192.168.1.29:45232): "))?,
        "2" => prompt(tr!("对端地址（例如 192.168.1.29:45232）: ", "Address of the other computer (for example 192.168.1.29:45232): "))?,
        _ => return Err(tr!("只能选择 1 或 2", "Choose only 1 or 2").to_owned()),
    };
    let mut address_argument = std::iter::once(address_text);
    let address = parse_private_socket(&mut address_argument, "address")?;
    let default_label = if role == "1" {
        tr!("另一台电脑", "Other computer")
    } else {
        tr!("主机", "Host")
    };
    let label = prompt(&format!("{} [{default_label}]: ", tr!("给对端起一个名称", "Name for the other computer")))?;
    let label = if label.is_empty() {
        default_label.to_owned()
    } else {
        label
    };
    if label.len() > 128 || label.chars().any(char::is_control) {
        return Err(tr!("设备名称必须为 1 到 128 个 UTF-8 字节且不能包含控制字符", "The device name must be 1 to 128 UTF-8 bytes and contain no control characters").to_owned());
    }
    println!("{}", tr!("\n正在建立配对，请保持两端窗口打开...", "\nEstablishing the pairing. Keep the windows on both computers open..."));
    let pending = if role == "1" {
        listen_for_pairing(store, address, Duration::from_mins(2))
            .map_err(|error| format!("pairing listener failed: {error}"))?
    } else {
        connect_for_pairing(store, address, Duration::from_mins(2))
            .map_err(|error| format!("pairing connection failed: {error}"))?
    };
    confirm_pairing(pending, &label)
}

fn run_trust_wizard() -> Result<(), String> {
    let result = run_trust_wizard_inner();
    if let Err(error) = &result {
        eprintln!("\n{}{error}", tr!("设备管理失败：", "Device management failed: "));
    }
    wait_for_enter(tr!("\n按 Enter 关闭此窗口...", "\nPress Enter to close this window..."))?;
    result
}

fn run_trust_wizard_inner() -> Result<(), String> {
    let store = DeviceStore::current_user()
        .map_err(|error| format!("device store unavailable: {error}"))?;
    let peers = store
        .list_peers()
        .map_err(|error| format!("trust list failed: {error}"))?;
    println!("{}", tr!("Pierdesk 已配对设备\n", "Pierdesk paired devices\n"));
    if peers.is_empty() {
        println!("{}", tr!("当前没有已配对设备。", "There are no paired devices."));
        return Ok(());
    }
    for (index, peer) in peers.iter().enumerate() {
        println!("[{}] {}\n    {}", index + 1, peer.label, peer.fingerprint);
    }
    println!("{}", tr!("[0] 仅查看，不做修改", "[0] View only, make no changes"));
    let selection = prompt(tr!("\n输入要取消配对的编号: ", "\nEnter the number of the device to unpair: "))?
        .parse::<usize>()
        .map_err(|_| tr!("请输入列表中的数字", "Enter a number from the list").to_owned())?;
    if selection == 0 {
        return Ok(());
    }
    let peer = peers
        .get(selection.saturating_sub(1))
        .ok_or_else(|| tr!("设备编号超出范围", "Device number out of range").to_owned())?;
    println!("\n{}{} ({})", tr!("即将取消配对：", "About to unpair: "), peer.label, peer.fingerprint);
    if prompt(tr!("输入大写 REVOKE 确认: ", "Type REVOKE in capitals to confirm: "))? != "REVOKE" {
        return Err(tr!("未输入大写 REVOKE，没有修改信任列表", "REVOKE was not typed in capitals; the trust list was not changed").to_owned());
    }
    store
        .revoke_peer(peer.fingerprint)
        .map_err(|error| format!("trust revocation failed: {error}"))?;
    println!("{}{}", tr!("已取消配对：", "Unpaired: "), peer.label);
    let _ = pierdesk::tray::reload_existing();
    Ok(())
}

fn run_settings_wizard() -> Result<(), String> {
    let result = run_settings_wizard_inner();
    if let Err(error) = &result {
        eprintln!("\n{}{error}", tr!("连接设置失败：", "Connection settings failed: "));
    }
    wait_for_enter(tr!("\n按 Enter 关闭此窗口...", "\nPress Enter to close this window..."))?;
    result
}

#[allow(clippy::too_many_lines)]
fn run_settings_wizard_inner() -> Result<(), String> {
    let store = DeviceStore::current_user()
        .map_err(|error| format!("device store unavailable: {error}"))?;
    let identity = store
        .load_or_create_identity()
        .map_err(|error| format!("device identity initialization failed: {error}"))?;
    let peers = store
        .list_peers()
        .map_err(|error| format!("trust list failed: {error}"))?;
    if peers.is_empty() {
        return Err(tr!("当前没有已配对设备，请先从托盘选择“配对新设备”", "There are no paired devices yet. Choose “Pair new device…” from the tray menu first").to_owned());
    }
    let previous = AppSettings::load(&store).ok();
    println!("{}", tr!("Pierdesk 局域网连接设置\n", "Pierdesk LAN connection settings\n"));
    println!("{}{}", tr!("本机指纹：", "Local fingerprint: "), identity.identity.fingerprint());
    println!("{}", tr!("请选择唯一的活动目标设备：", "Choose the single active target device:"));
    for (index, peer) in peers.iter().enumerate() {
        let marker = if previous
            .as_ref()
            .is_some_and(|settings| settings.active_peer == peer.fingerprint)
        {
            tr!("（当前）", "(current)")
        } else {
            ""
        };
        println!(
            "[{}] {} {}\n    {}",
            index + 1,
            peer.label,
            marker,
            peer.fingerprint
        );
    }
    let default_selection = previous
        .as_ref()
        .and_then(|settings| {
            peers
                .iter()
                .position(|peer| peer.fingerprint == settings.active_peer)
        })
        .map_or(1, |index| index + 1);
    let selection = prompt_with_default(tr!("设备编号", "Device number"), &default_selection.to_string())?
        .parse::<usize>()
        .map_err(|_| tr!("设备编号必须是数字", "The device number must be a number").to_owned())?;
    let peer = peers
        .get(selection.saturating_sub(1))
        .ok_or_else(|| tr!("设备编号超出范围", "Device number out of range").to_owned())?;

    println!("{}", tr!("\n每台电脑需要一个固定服务地址，建议端口 45233。", "\nEach computer needs a fixed service address; port 45233 is recommended."));
    println!("{}", tr!("本机地址填写本机真实局域网 IPv4；对端地址填写另一台电脑的地址。", "Enter this computer's real LAN IPv4 address as the local address, and the other computer's address as the peer address."));
    let local_default = previous.as_ref().map_or_else(
        || "192.168.1.2:45233".to_owned(),
        |value| value.local_endpoint.to_string(),
    );
    let local_text = prompt_with_default(tr!("本机服务地址", "Local service address"), &local_default)?;
    let local_endpoint = parse_private_socket_text(&local_text, tr!("本机服务地址", "Local service address"))?;
    let peer_default = previous.as_ref().map_or_else(
        || "192.168.1.3:45233".to_owned(),
        |value| value.peer_endpoint.to_string(),
    );
    let peer_text = prompt_with_default(tr!("对端服务地址", "Peer service address"), &peer_default)?;
    let peer_endpoint = parse_private_socket_text(&peer_text, tr!("对端服务地址", "Peer service address"))?;
    let auto_default = if previous.as_ref().is_some_and(|value| value.auto_receive) {
        "YES"
    } else {
        "NO"
    };
    let auto_receive = match prompt_with_default(
        tr!("收到该设备的文件清单后自动替换本机文件剪贴板（YES/NO）", "Automatically replace this computer's file clipboard when a manifest arrives from this device (YES/NO)"),
        auto_default,
    )?
    .as_str()
    {
        "YES" => true,
        "NO" => false,
        _ => return Err(tr!("自动接收设置只能输入大写 YES 或 NO", "Automatic receiving accepts only YES or NO in capitals").to_owned()),
    };
    let settings = AppSettings {
        local_endpoint,
        active_peer: peer.fingerprint,
        peer_endpoint,
        auto_receive,
    };
    settings
        .save(&store)
        .map_err(|error| format!("settings save failed: {error}"))?;
    println!(
        "\nSETTINGS saved=true peer={:?} local={} remote={} auto_receive={}",
        peer.label, local_endpoint, peer_endpoint, auto_receive
    );
    let _ = pierdesk::tray::reload_existing();
    Ok(())
}

fn run_settings_show() -> Result<(), String> {
    let store = DeviceStore::current_user()
        .map_err(|error| format!("device store unavailable: {error}"))?;
    let settings = AppSettings::load(&store)
        .map_err(|error| format!("connection settings unavailable: {error}"))?;
    let peer = store
        .load_peer(settings.active_peer)
        .map_err(|error| format!("active peer unavailable: {error}"))?;
    println!(
        "SETTINGS peer={:?} fingerprint={} local={} remote={} auto_receive={}",
        peer.label,
        peer.fingerprint,
        settings.local_endpoint,
        settings.peer_endpoint,
        settings.auto_receive
    );
    Ok(())
}

fn prompt(message: &str) -> Result<String, String> {
    print!("{message}");
    std::io::Write::flush(&mut std::io::stdout())
        .map_err(|error| format!("console output failed: {error}"))?;
    let mut value = String::new();
    std::io::stdin()
        .read_line(&mut value)
        .map_err(|error| format!("console input failed: {error}"))?;
    Ok(value.trim().to_owned())
}

fn prompt_with_default(label: &str, default: &str) -> Result<String, String> {
    let value = prompt(&format!("{label} [{default}]: "))?;
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value
    })
}

fn parse_private_socket_text(value: &str, label: &str) -> Result<std::net::SocketAddr, String> {
    let mut values = std::iter::once(value.to_owned());
    parse_private_socket(&mut values, label)
}

fn wait_for_enter(message: &str) -> Result<(), String> {
    let _ = prompt(message)?;
    Ok(())
}

#[derive(Default)]
struct TlsCliFiles {
    store: Option<std::path::PathBuf>,
    identity_certificate: Option<std::path::PathBuf>,
    identity_private_key: Option<std::path::PathBuf>,
    peer_certificate: Option<std::path::PathBuf>,
    peer_fingerprint: Option<CertificateFingerprint>,
}

impl TlsCliFiles {
    fn load(self) -> Result<(TlsIdentity, Vec<u8>, CertificateFingerprint), String> {
        if let Some(root) = self.store {
            if self.identity_certificate.is_some()
                || self.identity_private_key.is_some()
                || self.peer_certificate.is_some()
            {
                return Err(
                    "--store cannot be combined with --identity-cert, --identity-key, or --peer-cert"
                        .to_owned(),
                );
            }
            let peer_fingerprint = self
                .peer_fingerprint
                .ok_or_else(|| "--peer-fingerprint is required".to_owned())?;
            let store = DeviceStore::new(root);
            let identity = store
                .load_identity()
                .map_err(|error| format!("device identity load failed: {error}"))?;
            let peer = store
                .load_peer(peer_fingerprint)
                .map_err(|error| format!("trusted peer load failed: {error}"))?;
            return Ok((identity, peer.into_certificate_der(), peer_fingerprint));
        }
        let identity_certificate = self
            .identity_certificate
            .ok_or_else(|| "--identity-cert is required".to_owned())?;
        let identity_private_key = self
            .identity_private_key
            .ok_or_else(|| "--identity-key is required".to_owned())?;
        let peer_certificate_path = self
            .peer_certificate
            .ok_or_else(|| "--peer-cert is required".to_owned())?;
        let peer_fingerprint = self
            .peer_fingerprint
            .ok_or_else(|| "--peer-fingerprint is required".to_owned())?;
        let identity = TlsIdentity::load(&identity_certificate, &identity_private_key)
            .map_err(|error| format!("identity load failed: {error}"))?;
        let peer_certificate =
            load_and_verify_peer_certificate(&peer_certificate_path, peer_fingerprint)
                .map_err(|error| format!("peer pin verification failed: {error}"))?;
        Ok((identity, peer_certificate, peer_fingerprint))
    }

    fn load_source(
        self,
        timeout: Duration,
    ) -> Result<
        (
            SecureSourceTls,
            CertificateFingerprint,
            CertificateFingerprint,
        ),
        String,
    > {
        if let Some(root) = self.store {
            if self.identity_certificate.is_some()
                || self.identity_private_key.is_some()
                || self.peer_certificate.is_some()
            {
                return Err(
                    "--store cannot be combined with --identity-cert, --identity-key, or --peer-cert"
                        .to_owned(),
                );
            }
            let peer_fingerprint = self
                .peer_fingerprint
                .ok_or_else(|| "--peer-fingerprint is required".to_owned())?;
            let store = DeviceStore::new(root);
            let identity = store
                .load_identity()
                .map_err(|error| format!("device identity load failed: {error}"))?;
            store
                .load_peer(peer_fingerprint)
                .map_err(|error| format!("trusted peer load failed: {error}"))?;
            let local_fingerprint = identity.fingerprint();
            let tls = TrustedTlsServer::new(identity, store, timeout)
                .map_err(|error| format!("TLS server configuration failed: {error}"))?;
            return Ok((
                SecureSourceTls::Trusted {
                    tls,
                    authorized_peer: peer_fingerprint,
                },
                local_fingerprint,
                peer_fingerprint,
            ));
        }
        let (identity, peer_certificate, peer_fingerprint) = self.load()?;
        let local_fingerprint = identity.fingerprint();
        let tls = PinnedTlsServer::new(&identity, peer_certificate, peer_fingerprint, timeout)
            .map_err(|error| format!("TLS server configuration failed: {error}"))?;
        Ok((
            SecureSourceTls::Pinned(tls),
            local_fingerprint,
            peer_fingerprint,
        ))
    }
}

fn parse_store_only(mut arguments: impl Iterator<Item = String>) -> Result<DeviceStore, String> {
    let mut root = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    resolve_store(root)
}

fn parse_store_and_output(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(DeviceStore, std::path::PathBuf), String> {
    let mut root = None;
    let mut output = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            "--out" => output = Some(parse_path(&mut arguments, &argument)?),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((
        resolve_store(root)?,
        output.ok_or_else(|| "--out is required".to_owned())?,
    ))
}

fn parse_trust_import(
    mut arguments: impl Iterator<Item = String>,
) -> Result<
    (
        DeviceStore,
        std::path::PathBuf,
        CertificateFingerprint,
        String,
    ),
    String,
> {
    let mut root = None;
    let mut certificate = None;
    let mut fingerprint = None;
    let mut label = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            "--cert" => certificate = Some(parse_path(&mut arguments, &argument)?),
            "--fingerprint" => {
                fingerprint = Some(parse_fingerprint(&mut arguments, &argument)?);
            }
            "--label" => {
                label = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--label requires a value".to_owned())?,
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((
        resolve_store(root)?,
        certificate.ok_or_else(|| "--cert is required".to_owned())?,
        fingerprint.ok_or_else(|| "--fingerprint is required".to_owned())?,
        label.ok_or_else(|| "--label is required".to_owned())?,
    ))
}

fn parse_store_and_fingerprint(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(DeviceStore, CertificateFingerprint), String> {
    let mut root = None;
    let mut fingerprint = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            "--fingerprint" => {
                fingerprint = Some(parse_fingerprint(&mut arguments, &argument)?);
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((
        resolve_store(root)?,
        fingerprint.ok_or_else(|| "--fingerprint is required".to_owned())?,
    ))
}

fn resolve_store(root: Option<std::path::PathBuf>) -> Result<DeviceStore, String> {
    root.map_or_else(
        || {
            DeviceStore::current_user()
                .map_err(|error| format!("device store unavailable: {error}"))
        },
        |root| Ok(DeviceStore::new(root)),
    )
}

struct ParsedSecureSource {
    listen_address: std::net::SocketAddr,
    source_paths: Vec<std::path::PathBuf>,
    offer_ttl: Duration,
    transfer_ttl: Duration,
    io_timeout: Duration,
    lifetime: Option<Duration>,
    tls: TlsCliFiles,
}

struct ParsedSecureClient {
    connect_address: std::net::SocketAddr,
    output_path: Option<std::path::PathBuf>,
    io_timeout: Duration,
    recovery_window: Duration,
    lifetime: Option<Duration>,
    async_mode: bool,
    auto_cancel: bool,
    tls: TlsCliFiles,
}

struct PairingCliOptions {
    address: std::net::SocketAddr,
    store: DeviceStore,
    label: String,
    timeout: Duration,
}

fn parse_pairing_options(
    mut arguments: impl Iterator<Item = String>,
    endpoint_option: &str,
) -> Result<PairingCliOptions, String> {
    let mut address = None;
    let mut root = None;
    let mut label = None;
    let mut timeout_seconds = 120_u64;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            value if value == endpoint_option => {
                address = Some(parse_private_socket(&mut arguments, &argument)?);
            }
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            "--label" => {
                label = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--label requires a value".to_owned())?,
                );
            }
            "--timeout-seconds" => {
                timeout_seconds = parse_value(&mut arguments, &argument)?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=300).contains(&timeout_seconds) {
        return Err("--timeout-seconds must be between 1 and 300".to_owned());
    }
    let label = label.ok_or_else(|| "--label is required".to_owned())?;
    let trimmed_label = label.trim();
    if trimmed_label.is_empty()
        || trimmed_label.len() > 128
        || trimmed_label.chars().any(char::is_control)
    {
        return Err(
            "--label must contain 1 to 128 UTF-8 bytes without control characters".to_owned(),
        );
    }
    Ok(PairingCliOptions {
        address: address.ok_or_else(|| format!("{endpoint_option} is required"))?,
        store: resolve_store(root)?,
        label: trimmed_label.to_owned(),
        timeout: Duration::from_secs(timeout_seconds),
    })
}

fn confirm_pairing(pending: PendingPairing, label: &str) -> Result<(), String> {
    println!(
        "PAIR verify_code={} local_fingerprint={} peer_fingerprint={} peer_address={}",
        pending.code(),
        pending.local_fingerprint(),
        pending.peer_fingerprint(),
        pending.peer_address()
    );
    println!("Compare verify_code on both devices. Type YES on both devices to approve:");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("pairing confirmation input failed: {error}"))?;
    let approved = answer.trim() == "YES";
    let peer = pending
        .confirm(approved, label)
        .map_err(|error| format!("pairing confirmation failed: {error}"))?;
    println!(
        "PAIRED completed=true fingerprint={} label={:?}",
        peer.fingerprint, peer.label
    );
    let _ = pierdesk::tray::reload_existing();
    Ok(())
}

fn parse_identity_generation_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    let mut certificate = None;
    let mut private_key = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--cert-out" => certificate = Some(parse_path(&mut arguments, &argument)?),
            "--key-out" => private_key = Some(parse_path(&mut arguments, &argument)?),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((
        certificate.ok_or_else(|| "--cert-out is required".to_owned())?,
        private_key.ok_or_else(|| "--key-out is required".to_owned())?,
    ))
}

fn parse_secure_source_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<ParsedSecureSource, String> {
    let mut listen_address = None;
    let mut source_paths = Vec::new();
    let mut offer_ttl_seconds = 900_u64;
    let mut transfer_ttl_seconds = 3600_u64;
    let mut io_timeout_seconds = 30_u64;
    let mut lifetime = None;
    let mut tls = TlsCliFiles::default();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                listen_address = Some(parse_private_socket(&mut arguments, &argument)?);
            }
            "--file" => source_paths.push(parse_path(&mut arguments, &argument)?),
            "--offer-ttl-seconds" => {
                offer_ttl_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--transfer-ttl-seconds" => {
                transfer_ttl_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--io-timeout-seconds" => {
                io_timeout_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--lifetime-seconds" => {
                lifetime = Some(Duration::from_secs(parse_value(&mut arguments, &argument)?));
            }
            "--store" => {
                tls.store = Some(parse_path(&mut arguments, &argument)?);
            }
            "--identity-cert" => {
                tls.identity_certificate = Some(parse_path(&mut arguments, &argument)?);
            }
            "--identity-key" => {
                tls.identity_private_key = Some(parse_path(&mut arguments, &argument)?);
            }
            "--peer-cert" => {
                tls.peer_certificate = Some(parse_path(&mut arguments, &argument)?);
            }
            "--peer-fingerprint" => {
                tls.peer_fingerprint = Some(parse_fingerprint(&mut arguments, &argument)?);
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if offer_ttl_seconds == 0 || transfer_ttl_seconds == 0 || io_timeout_seconds == 0 {
        return Err("secure TTL and timeout values must be greater than zero".to_owned());
    }
    Ok(ParsedSecureSource {
        listen_address: listen_address.ok_or_else(|| "--listen is required".to_owned())?,
        source_paths: (!source_paths.is_empty())
            .then_some(source_paths)
            .ok_or_else(|| "at least one --file is required".to_owned())?,
        offer_ttl: Duration::from_secs(offer_ttl_seconds),
        transfer_ttl: Duration::from_secs(transfer_ttl_seconds),
        io_timeout: Duration::from_secs(io_timeout_seconds),
        lifetime,
        tls,
    })
}

fn parse_secure_client_options(
    mut arguments: impl Iterator<Item = String>,
    allow_output: bool,
    allow_auto_cancel: bool,
) -> Result<ParsedSecureClient, String> {
    let mut connect_address = None;
    let mut output_path = None;
    let mut io_timeout_seconds = 30_u64;
    let mut recovery_seconds = 120_u64;
    let mut lifetime = None;
    let mut async_mode = false;
    let mut auto_cancel = false;
    let mut tls = TlsCliFiles::default();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--connect" => {
                connect_address = Some(parse_private_socket(&mut arguments, &argument)?);
            }
            "--output" if allow_output => {
                output_path = Some(parse_path(&mut arguments, &argument)?);
            }
            "--io-timeout-seconds" => {
                io_timeout_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--recovery-seconds" => {
                recovery_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--lifetime-seconds" if !allow_output => {
                lifetime = Some(Duration::from_secs(parse_value(&mut arguments, &argument)?));
            }
            "--async-mode" if !allow_output => async_mode = true,
            "--auto-cancel" if allow_auto_cancel => auto_cancel = true,
            "--store" => {
                tls.store = Some(parse_path(&mut arguments, &argument)?);
            }
            "--identity-cert" => {
                tls.identity_certificate = Some(parse_path(&mut arguments, &argument)?);
            }
            "--identity-key" => {
                tls.identity_private_key = Some(parse_path(&mut arguments, &argument)?);
            }
            "--peer-cert" => {
                tls.peer_certificate = Some(parse_path(&mut arguments, &argument)?);
            }
            "--peer-fingerprint" => {
                tls.peer_fingerprint = Some(parse_fingerprint(&mut arguments, &argument)?);
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if io_timeout_seconds == 0 {
        return Err("--io-timeout-seconds must be greater than zero".to_owned());
    }
    if recovery_seconds > 3600 {
        return Err("--recovery-seconds must be between 0 and 3600".to_owned());
    }
    if allow_output && output_path.is_none() {
        return Err("--output is required".to_owned());
    }
    Ok(ParsedSecureClient {
        connect_address: connect_address.ok_or_else(|| "--connect is required".to_owned())?,
        output_path,
        io_timeout: Duration::from_secs(io_timeout_seconds),
        recovery_window: Duration::from_secs(recovery_seconds),
        lifetime,
        async_mode,
        auto_cancel,
        tls,
    })
}

fn build_secure_client(parsed: &ParsedSecureClient) -> Result<SecureOfferClient, String> {
    let tls_files = TlsCliFiles {
        store: parsed.tls.store.clone(),
        identity_certificate: parsed.tls.identity_certificate.clone(),
        identity_private_key: parsed.tls.identity_private_key.clone(),
        peer_certificate: parsed.tls.peer_certificate.clone(),
        peer_fingerprint: parsed.tls.peer_fingerprint,
    };
    let (identity, peer_certificate, peer_fingerprint) = tls_files.load()?;
    println!(
        "IDENTITY local_fingerprint={} pinned_peer={peer_fingerprint}",
        identity.fingerprint()
    );
    let tls = PinnedTlsClient::new(
        &identity,
        peer_certificate,
        peer_fingerprint,
        parsed.io_timeout,
    )
    .map_err(|error| format!("TLS client configuration failed: {error}"))?;
    Ok(SecureOfferClient::with_recovery_policy(
        parsed.connect_address,
        tls,
        SecureRecoveryPolicy {
            max_elapsed: parsed.recovery_window,
            connect_attempt_timeout: Duration::from_secs(2),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
        },
    ))
}

fn parse_path(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<std::path::PathBuf, String> {
    arguments
        .next()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| format!("{option} requires a path"))
}

fn parse_fingerprint(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<CertificateFingerprint, String> {
    let value = arguments
        .next()
        .ok_or_else(|| format!("{option} requires a SHA-256 fingerprint"))?;
    CertificateFingerprint::from_str(&value)
        .map_err(|error| format!("invalid value for {option}: {error}"))
}

fn parse_private_socket(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<std::net::SocketAddr, String> {
    let value = arguments
        .next()
        .ok_or_else(|| format!("{option} requires an address"))?;
    let address = std::net::SocketAddr::from_str(&value)
        .map_err(|error| format!("invalid value for {option}: {error}"))?;
    let allowed = match address.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        std::net::IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    };
    if !allowed {
        return Err(format!(
            "{option} must use a loopback or private unicast address"
        ));
    }
    Ok(address)
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len().saturating_mul(2)),
        |mut text, byte| {
            let _ = write!(text, "{byte:02X}");
            text
        },
    )
}

fn parse_file_capture_probe_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<FileCaptureProbeOptions, String> {
    let mut offer_ttl_seconds = 300_u64;
    let mut lifetime = None;
    let mut async_mode = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--async-mode" => async_mode = true,
            "--offer-ttl-seconds" => {
                offer_ttl_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--lifetime-seconds" => {
                lifetime = Some(Duration::from_secs(parse_value(&mut arguments, &argument)?));
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if offer_ttl_seconds == 0 {
        return Err("--offer-ttl-seconds must be greater than zero".to_owned());
    }
    Ok(FileCaptureProbeOptions {
        offer_ttl: Duration::from_secs(offer_ttl_seconds),
        lifetime,
        async_mode,
    })
}

fn parse_source_drag_probe_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<SourceDragProbeOptions, String> {
    let mut edge = None;
    let mut thickness = 8_u32;
    let mut lifetime_seconds = 120_u64;
    let mut settle_ms = 500_u64;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--edge" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--edge requires left, right, top, or bottom".to_owned())?;
                edge = Some(match value.as_str() {
                    "left" => DragProbeEdge::Left,
                    "right" => DragProbeEdge::Right,
                    "top" => DragProbeEdge::Top,
                    "bottom" => DragProbeEdge::Bottom,
                    _ => return Err("--edge requires left, right, top, or bottom".to_owned()),
                });
            }
            "--thickness" => {
                thickness = arguments
                    .next()
                    .ok_or_else(|| "--thickness requires a value".to_owned())?
                    .parse::<u32>()
                    .map_err(|error| format!("invalid --thickness: {error}"))?;
            }
            "--lifetime-seconds" => {
                lifetime_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--settle-ms" => settle_ms = parse_value(&mut arguments, &argument)?,
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=64).contains(&thickness) {
        return Err("--thickness must be between 1 and 64".to_owned());
    }
    if !(1..=300).contains(&lifetime_seconds) {
        return Err("--lifetime-seconds must be between 1 and 300".to_owned());
    }
    if !(1..=5_000).contains(&settle_ms) {
        return Err("--settle-ms must be between 1 and 5000".to_owned());
    }
    Ok(SourceDragProbeOptions {
        edge: edge.ok_or_else(|| "--edge is required".to_owned())?,
        thickness,
        lifetime: Duration::from_secs(lifetime_seconds),
        settle_delay: Duration::from_millis(settle_ms),
    })
}

fn parse_drag_edge_helper_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(u16, String, DragProbeEdge, u32), String> {
    let mut port = None;
    let mut token = None;
    let mut edge = None;
    let mut thickness = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--port" => {
                port = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--port requires a value".to_owned())?
                        .parse::<u16>()
                        .map_err(|error| format!("invalid --port: {error}"))?,
                );
            }
            "--token" => {
                token = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--token requires a value".to_owned())?,
                );
            }
            "--edge" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--edge requires left, right, top, or bottom".to_owned())?;
                edge = Some(match value.as_str() {
                    "left" => DragProbeEdge::Left,
                    "right" => DragProbeEdge::Right,
                    "top" => DragProbeEdge::Top,
                    "bottom" => DragProbeEdge::Bottom,
                    _ => return Err("--edge requires left, right, top, or bottom".to_owned()),
                });
            }
            "--thickness" => {
                thickness = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--thickness requires a value".to_owned())?
                        .parse::<u32>()
                        .map_err(|error| format!("invalid --thickness: {error}"))?,
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    let port = port
        .filter(|port| *port != 0)
        .ok_or_else(|| "--port is required".to_owned())?;
    let token = token.ok_or_else(|| "--token is required".to_owned())?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("--token must contain 64 hexadecimal characters".to_owned());
    }
    let thickness = thickness
        .filter(|thickness| (1..=128).contains(thickness))
        .ok_or_else(|| "--thickness must be between 1 and 128".to_owned())?;
    Ok((
        port,
        token,
        edge.ok_or_else(|| "--edge is required".to_owned())?,
        thickness,
    ))
}

fn parse_target_drag_probe_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<TargetDragProbeOptions, String> {
    let mut file = None;
    let mut lifetime_seconds = 120_u64;
    let mut auto_cancel = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--file" => file = Some(parse_path(&mut arguments, &argument)?),
            "--lifetime-seconds" => {
                lifetime_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--auto-cancel" => auto_cancel = true,
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=300).contains(&lifetime_seconds) {
        return Err("--lifetime-seconds must be between 1 and 300".to_owned());
    }
    let file = file.ok_or_else(|| "--file is required".to_owned())?;
    if !file.is_absolute() {
        return Err("--file must be an absolute path".to_owned());
    }
    Ok(TargetDragProbeOptions {
        file,
        lifetime: Duration::from_secs(lifetime_seconds),
        auto_cancel,
    })
}

fn parse_loopback_probe_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<LoopbackProbeOptions, String> {
    let mut size_mib = 64_u64;
    let mut range_kib = 64_u64;
    let mut fragment_bytes = 8 * 1024_u64;
    let mut delay_ms = 1_u64;
    let mut io_timeout_seconds = 30_u64;
    let mut lifetime = None;
    let mut async_mode = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--async-mode" => async_mode = true,
            "--size-mib" => size_mib = parse_value(&mut arguments, &argument)?,
            "--range-kib" => range_kib = parse_value(&mut arguments, &argument)?,
            "--fragment-bytes" => {
                fragment_bytes = parse_value(&mut arguments, &argument)?;
            }
            "--delay-ms" => delay_ms = parse_value(&mut arguments, &argument)?,
            "--io-timeout-seconds" => {
                io_timeout_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--lifetime-seconds" => {
                lifetime = Some(Duration::from_secs(parse_value(&mut arguments, &argument)?));
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if range_kib == 0 || fragment_bytes == 0 || io_timeout_seconds == 0 {
        return Err(
            "--range-kib, --fragment-bytes and --io-timeout-seconds must be greater than zero"
                .to_owned(),
        );
    }

    let size_bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "--size-mib is too large".to_owned())?;
    let range_bytes = range_kib
        .checked_mul(1024)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| "--range-kib is too large".to_owned())?;
    let fragment_bytes =
        usize::try_from(fragment_bytes).map_err(|_| "--fragment-bytes is too large".to_owned())?;
    Ok(LoopbackProbeOptions {
        size_bytes,
        range_bytes,
        fragment_bytes,
        range_delay: Duration::from_millis(delay_ms),
        connect_timeout: Duration::from_secs(2),
        io_timeout: Duration::from_secs(io_timeout_seconds),
        lifetime,
        async_mode,
    })
}

fn parse_pause_probe_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<PauseProbeOptions, String> {
    let mut size_mib = 64_u64;
    let mut chunk_kib = 64_u64;
    let mut delay_ms = 8_u64;
    let mut lifetime = None;
    let mut async_mode = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--async-mode" => async_mode = true,
            "--size-mib" => size_mib = parse_value(&mut arguments, &argument)?,
            "--chunk-kib" => chunk_kib = parse_value(&mut arguments, &argument)?,
            "--delay-ms" => delay_ms = parse_value(&mut arguments, &argument)?,
            "--lifetime-seconds" => {
                lifetime = Some(Duration::from_secs(parse_value(&mut arguments, &argument)?));
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if size_mib == 0 || chunk_kib == 0 {
        return Err("--size-mib and --chunk-kib must be greater than zero".to_owned());
    }

    let size_bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "--size-mib is too large".to_owned())?;
    let chunk_bytes = chunk_kib
        .checked_mul(1024)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| "--chunk-kib is too large".to_owned())?;
    Ok(PauseProbeOptions {
        size_bytes,
        chunk_bytes,
        chunk_delay: Duration::from_millis(delay_ms),
        lifetime,
        async_mode,
    })
}

fn parse_cross_screen_input_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<InputDiagnosticOptions, String> {
    let mut lifetime_seconds = 15_u64;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--lifetime-seconds" => {
                lifetime_seconds = parse_value(&mut arguments, &argument)?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=300).contains(&lifetime_seconds) {
        return Err("--lifetime-seconds must be between 1 and 300".to_owned());
    }
    Ok(InputDiagnosticOptions {
        lifetime: Duration::from_secs(lifetime_seconds),
    })
}

fn parse_cross_screen_channel_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<ChannelDiagnosticOptions, String> {
    let mut role = None;
    let mut peer_fingerprint = None;
    let mut root = None;
    let mut connect_timeout_seconds = 60_u64;
    let mut lifetime_seconds = 30_u64;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                if role.is_some() {
                    return Err("exactly one of --listen or --connect is required".to_owned());
                }
                role = Some(ChannelDiagnosticRole::Listen(parse_private_socket(
                    &mut arguments,
                    &argument,
                )?));
            }
            "--connect" => {
                if role.is_some() {
                    return Err("exactly one of --listen or --connect is required".to_owned());
                }
                role = Some(ChannelDiagnosticRole::Connect(parse_private_socket(
                    &mut arguments,
                    &argument,
                )?));
            }
            "--peer-fingerprint" => {
                peer_fingerprint = Some(parse_fingerprint(&mut arguments, &argument)?);
            }
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            "--connect-timeout-seconds" => {
                connect_timeout_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--lifetime-seconds" => {
                lifetime_seconds = parse_value(&mut arguments, &argument)?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=300).contains(&connect_timeout_seconds) {
        return Err("--connect-timeout-seconds must be between 1 and 300".to_owned());
    }
    if !(1..=300).contains(&lifetime_seconds) {
        return Err("--lifetime-seconds must be between 1 and 300".to_owned());
    }
    Ok(ChannelDiagnosticOptions {
        store: resolve_store(root)?,
        peer_fingerprint: peer_fingerprint
            .ok_or_else(|| "--peer-fingerprint is required".to_owned())?,
        role: role.ok_or_else(|| "exactly one of --listen or --connect is required".to_owned())?,
        connect_timeout: Duration::from_secs(connect_timeout_seconds),
        lifetime: Duration::from_secs(lifetime_seconds),
    })
}

fn parse_cross_screen_automatic_options(
    mut arguments: impl Iterator<Item = String>,
) -> Result<AutomaticChannelDiagnosticOptions, String> {
    let mut root = None;
    let mut connect_timeout_seconds = 60_u64;
    let mut lifetime_seconds = 30_u64;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--store" => root = Some(parse_path(&mut arguments, &argument)?),
            "--connect-timeout-seconds" => {
                connect_timeout_seconds = parse_value(&mut arguments, &argument)?;
            }
            "--lifetime-seconds" => {
                lifetime_seconds = parse_value(&mut arguments, &argument)?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=300).contains(&connect_timeout_seconds) {
        return Err("--connect-timeout-seconds must be between 1 and 300".to_owned());
    }
    if !(1..=300).contains(&lifetime_seconds) {
        return Err("--lifetime-seconds must be between 1 and 300".to_owned());
    }
    Ok(AutomaticChannelDiagnosticOptions {
        store: resolve_store(root)?,
        connect_timeout: Duration::from_secs(connect_timeout_seconds),
        lifetime: Duration::from_secs(lifetime_seconds),
    })
}

fn parse_value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<u64, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value"))?
        .parse::<u64>()
        .map_err(|error| format!("invalid value for {option}: {error}"))
}

#[cfg(feature = "licensing")]
fn run_license_command(
    command: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<(), String> {
    let root = arguments.next().ok_or("license store directory required")?;
    let issuer = arguments.next().ok_or("issuer public key required")?;
    let client = pierdesk::licensing::LicenseClient::new(root, issuer)
        .map_err(|error| error.to_string())?;
    match command {
        "license-device" => {
            reject_remaining_arguments(arguments, command)?;
            println!(
                "device_public_key={}",
                client
                    .device_public_key()
                    .map_err(|error| error.to_string())?
            );
        }
        "license-status" => {
            reject_remaining_arguments(arguments, command)?;
            let license = client.status().map_err(|error| error.to_string())?;
            println!(
                "license_id={} offline_until={}",
                license.claims.license_id, license.claims.offline_until
            );
        }
        "license-release" => {
            let endpoint = arguments.next().ok_or("server URL required")?;
            reject_remaining_arguments(arguments, command)?;
            client
                .release(&endpoint)
                .map_err(|error| error.to_string())?;
            println!("released");
        }
        "license-activate" | "license-refresh" => {
            let endpoint = arguments.next().ok_or("server URL required")?;
            reject_remaining_arguments(arguments, command)?;
            let license = if command == "license-activate" {
                let stdin = std::io::stdin();
                let mut input = String::new();
                stdin
                    .lock()
                    .take(129)
                    .read_line(&mut input)
                    .map_err(|error| error.to_string())?;
                let code = input.trim().to_ascii_lowercase();
                if code.len() != 64 || !code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err("expected a 64-character activation code on stdin".to_owned());
                }
                client
                    .activate(&endpoint, &code)
                    .map_err(|error| error.to_string())?
            } else {
                client
                    .refresh(&endpoint)
                    .map_err(|error| error.to_string())?
            };
            println!(
                "license_id={} offline_until={}",
                license.claims.license_id, license.claims.offline_until
            );
        }
        _ => return Err("unknown license command".to_owned()),
    }
    Ok(())
}
