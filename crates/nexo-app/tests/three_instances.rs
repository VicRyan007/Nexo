//! Exercita uma chamada com tres instancias completas do Nexo.
//!
//! O teste nao depende de microfone ou camera fisicos: ele valida o ciclo de
//! vida da chamada, a sinalizacao WebRTC e a atualizacao dos controles da UI.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_compat::Compat;
use slint::Model;

async fn wait_for(
    what: &str,
    timeout: Duration,
    mut ready: impl FnMut() -> bool,
    mut snapshot: impl FnMut() -> String,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timeout aguardando {what}; estado atual: {}", snapshot());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn connected_participants(app: &nexo_app::AppInstance) -> usize {
    (0..app.window.get_participants().row_count())
        .filter(|index| {
            app.window
                .get_participants()
                .row_data(*index)
                .is_some_and(|participant| participant.connected)
        })
        .count()
}

#[allow(clippy::too_many_lines)]
async fn run_scenario() -> anyhow::Result<()> {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let dirs = (['a', 'b', 'c']).map(|suffix| {
        let path = std::env::temp_dir().join(format!("nexo-app-three-{suffix}-{unique}"));
        std::fs::create_dir_all(&path).map(|()| path)
    });
    let [dir_a, dir_b, dir_c] = dirs
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("diretorios temporarios incompletos"))?;

    let mut app_a = nexo_app::start_app_without_camera(&dir_a).context("iniciar instancia A")?;
    let mut app_b = nexo_app::start_app_without_camera(&dir_b).context("iniciar instancia B")?;
    let mut app_c = nexo_app::start_app_without_camera(&dir_c).context("iniciar instancia C")?;

    let invite_code = {
        let mut code = slint::SharedString::new();
        wait_for(
            "geracao do convite na instancia A",
            Duration::from_secs(60),
            || {
                app_a.window.invoke_create_network("Rede de tres".into());
                code = app_a.window.get_invite_code();
                !code.is_empty()
            },
            || {
                format!(
                    "A status={} peer={}",
                    app_a.window.get_status_text(),
                    app_a.window.get_peer_id()
                )
            },
        )
        .await?;
        anyhow::ensure!(code.starts_with("NEXO1."), "convite inesperado: {code}");
        code.to_string()
    };

    app_b.window.invoke_join_network(invite_code.clone().into());
    app_c.window.invoke_join_network(invite_code.into());
    wait_for(
        "B e C aceitarem o convite",
        Duration::from_secs(45),
        || app_b.window.get_has_community() && app_c.window.get_has_community(),
        || {
            format!(
                "B={} | C={}",
                app_b.window.get_status_text(),
                app_c.window.get_status_text()
            )
        },
    )
    .await?;

    app_a.window.invoke_join_call();
    wait_for(
        "A iniciar a chamada",
        Duration::from_secs(30),
        || app_a.window.get_call_active(),
        || format!("A={}", app_a.window.get_call_status()),
    )
    .await?;

    app_b.window.invoke_join_call();
    wait_for(
        "A e B estabelecerem a primeira conexao",
        Duration::from_secs(90),
        || {
            app_a
                .window
                .get_call_status()
                .as_str()
                .contains("1 pessoa(s) conectada(s)")
        },
        || {
            format!(
                "A={} | B={}",
                app_a.window.get_call_status(),
                app_b.window.get_call_status()
            )
        },
    )
    .await?;

    app_c.window.invoke_join_call();
    wait_for(
        "tres participantes se conectarem em A",
        Duration::from_secs(120),
        || connected_participants(&app_a) >= 2,
        || {
            format!(
                "A status={} participantes={} | B={} | C={}",
                app_a.window.get_call_status(),
                connected_participants(&app_a),
                app_b.window.get_call_status(),
                app_c.window.get_call_status()
            )
        },
    )
    .await?;
    wait_for(
        "tres participantes se conectarem em B",
        Duration::from_secs(60),
        || connected_participants(&app_b) >= 2,
        || {
            format!(
                "A={} | B={} | C={}",
                app_a.window.get_call_status(),
                app_b.window.get_call_status(),
                app_c.window.get_call_status()
            )
        },
    )
    .await?;
    wait_for(
        "tres participantes se conectarem em C",
        Duration::from_secs(60),
        || connected_participants(&app_c) >= 2,
        || {
            format!(
                "A={} | B={} | C={}",
                app_a.window.get_call_status(),
                app_b.window.get_call_status(),
                app_c.window.get_call_status()
            )
        },
    )
    .await?;

    wait_for(
        "as identidades de rede ficarem disponiveis",
        Duration::from_secs(10),
        || {
            app_a.network_peer_id().is_some()
                && app_b.network_peer_id().is_some()
                && app_c.network_peer_id().is_some()
        },
        || {
            format!(
                "A={:?} B={:?} C={:?}",
                app_a.network_peer_id(),
                app_b.network_peer_id(),
                app_c.network_peer_id()
            )
        },
    )
    .await?;

    let peer_a = app_a
        .network_peer_id()
        .context("identidade de rede ausente em A")?;
    let peer_b = app_b
        .network_peer_id()
        .context("identidade de rede ausente em B")?;
    let peer_c = app_c
        .network_peer_id()
        .context("identidade de rede ausente em C")?;
    let mut elected_peer = None;
    wait_for(
        "um relay ser eleito",
        Duration::from_secs(20),
        || {
            let relays = [
                app_a.active_relay_peer_id(),
                app_b.active_relay_peer_id(),
                app_c.active_relay_peer_id(),
            ];
            if relays[0].is_some() && relays.iter().all(|relay| *relay == relays[0]) {
                elected_peer.clone_from(&relays[0]);
                return true;
            }
            false
        },
        || {
            format!(
                "A relay={:?} | B relay={:?} | C relay={:?}",
                app_a.active_relay_peer_id(),
                app_b.active_relay_peer_id(),
                app_c.active_relay_peer_id()
            )
        },
    )
    .await?;

    let relay_peer = elected_peer.context("nao foi possivel identificar o relay eleito")?;

    // Os controles precisam ser responsivos mesmo enquanto a thread de media
    // negocia codecs e troca participantes.
    anyhow::ensure!(
        app_b.window.get_video_enabled(),
        "camera de B deveria iniciar ativa"
    );
    app_b.window.invoke_toggle_video();
    anyhow::ensure!(
        !app_b.window.get_video_enabled(),
        "B nao desativou a camera imediatamente"
    );
    app_b.window.invoke_toggle_video();
    anyhow::ensure!(
        app_b.window.get_video_enabled(),
        "B nao reativou a camera imediatamente"
    );
    app_c.window.invoke_set_muted(true);
    anyhow::ensure!(
        app_c.window.get_call_muted(),
        "C nao aplicou o mute imediatamente"
    );

    // Stop exactly the elected relay. The remaining peers must detect the
    // disconnect and keep the call alive while promoting another participant.
    match relay_peer.as_str() {
        peer if peer == peer_a => app_a.shutdown(),
        peer if peer == peer_b => app_b.shutdown(),
        peer if peer == peer_c => app_c.shutdown(),
        _ => anyhow::bail!("relay eleito nao pertence as tres instancias"),
    }
    let mut migrated_relay = None;
    wait_for(
        "a migracao do relay apos a queda do host",
        Duration::from_secs(30),
        || {
            let survivors = [&app_a, &app_b, &app_c]
                .into_iter()
                .filter(|app| app.network_peer_id().as_deref() != Some(relay_peer.as_str()))
                .collect::<Vec<_>>();
            let relays = survivors
                .iter()
                .map(|app| app.active_relay_peer_id())
                .collect::<Vec<_>>();
            if relays.len() == 2
                && relays[0].is_some()
                && relays.iter().all(|relay| *relay == relays[0])
                && relays[0].as_deref() != Some(relay_peer.as_str())
                && survivors
                    .iter()
                    .all(|app| connected_participants(app) >= 1)
            {
                migrated_relay.clone_from(&relays[0]);
                return true;
            }
            false
        },
        || {
            format!(
                "A relay={:?} participantes={} | B relay={:?} participantes={} | C relay={:?} participantes={}",
                app_a.active_relay_peer_id(),
                connected_participants(&app_a),
                app_b.active_relay_peer_id(),
                connected_participants(&app_b),
                app_c.active_relay_peer_id(),
                connected_participants(&app_c)
            )
        },
    )
    .await?;
    let migrated_relay = migrated_relay.context("relay novo nao foi identificado")?;
    anyhow::ensure!(
        migrated_relay != relay_peer,
        "a migracao manteve o relay antigo"
    );
    for app in [&app_a, &app_b, &app_c] {
        if app.network_peer_id().as_deref() != Some(relay_peer.as_str()) {
            anyhow::ensure!(
                app.active_relay_peer_id().as_deref() == Some(migrated_relay.as_str()),
                "os participantes sobreviventes elegeram relays diferentes: esperado={migrated_relay}, atual={:?}",
                app.active_relay_peer_id()
            );
            anyhow::ensure!(
                connected_participants(app) >= 1,
                "a chamada nao permaneceu conectada apos a migracao"
            );
        }
    }

    if relay_peer != peer_a {
        app_a.window.invoke_leave_call();
    }
    if relay_peer != peer_b {
        app_b.window.invoke_leave_call();
    }
    if relay_peer != peer_c {
        app_c.window.invoke_leave_call();
    }
    wait_for(
        "as tres chamadas encerrarem",
        Duration::from_secs(30),
        || {
            (relay_peer == peer_a || !app_a.window.get_call_active())
                && (relay_peer == peer_b || !app_b.window.get_call_active())
                && (relay_peer == peer_c || !app_c.window.get_call_active())
        },
        || {
            format!(
                "A={} B={} C={}",
                app_a.window.get_call_status(),
                app_b.window.get_call_status(),
                app_c.window.get_call_status()
            )
        },
    )
    .await?;

    app_a.shutdown();
    app_b.shutdown();
    app_c.shutdown();
    drop(app_a);
    drop(app_b);
    drop(app_c);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
    let _ = std::fs::remove_dir_all(dir_c);
    slint::quit_event_loop()
        .map_err(|error| anyhow::anyhow!("ao encerrar o event loop: {error}"))?;
    Ok(())
}

#[test]
fn three_instances_share_a_call_without_closing_the_app() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let scenario_failed = Arc::new(AtomicBool::new(false));
    let scenario_failed_task = Arc::clone(&scenario_failed);
    slint::spawn_local(Compat::new(async move {
        if let Err(error) = run_scenario().await {
            eprintln!("three-instances scenario failed: {error:#}");
            scenario_failed_task.store(true, Ordering::Release);
            let _ = slint::quit_event_loop();
        }
    }))
    .expect("spawn scenario on the Slint event loop");
    slint::run_event_loop().expect("run Slint event loop");
    assert!(
        !scenario_failed.load(Ordering::Acquire),
        "three-instances scenario failed; see the diagnostic above"
    );
}
