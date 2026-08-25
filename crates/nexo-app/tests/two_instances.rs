//! Marco 1 do handoff: teste de aplicação com duas instâncias Nexo no mesmo
//! processo, usando o backend de teste do Slint. Cobre:
//!   * UI de convite (criar comunidade e gerar convite, aceitar convite);
//!   * conexão tardia (B entra numa comunidade que já tem histórico);
//!   * controles de chamada (entrar, silenciar, sair da voz);
//!   * estado de participante conectado via WebRTC real;
//!   * encerramento limpo das duas instâncias.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_compat::Compat;
use slint::Model;

/// Aguarda até que `ready` seja verdadeiro, verificando a cada 50 ms. Em caso
/// de timeout, inclui `snapshot()` na mensagem de erro para facilitar o debug.
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

fn messages_contain(model: &slint::ModelRc<nexo_app::MessageRow>, body: &str) -> bool {
    (0..model.row_count()).any(|index| {
        model
            .row_data(index)
            .is_some_and(|row| row.body.as_str() == body)
    })
}

fn direct_messages_contain(model: &slint::ModelRc<nexo_app::MessageRow>, body: &str) -> bool {
    messages_contain(model, body)
}

#[allow(clippy::too_many_lines)]
async fn run_scenario() -> anyhow::Result<()> {
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let dir_a = std::env::temp_dir().join(format!("nexo-app-two-a-{unique}"));
    let dir_b = std::env::temp_dir().join(format!("nexo-app-two-b-{unique}"));
    std::fs::create_dir_all(&dir_a)?;
    std::fs::create_dir_all(&dir_b)?;

    let mut app_a = nexo_app::start_app(&dir_a).context("iniciar instância A")?;
    let mut app_b = nexo_app::start_app(&dir_b).context("iniciar instância B")?;

    // 1) UI de convite: A cria a comunidade e recebe o código de convite.
    let invite_code = {
        let mut code = slint::SharedString::new();
        wait_for(
            "geração do convite na instância A",
            Duration::from_secs(60),
            || {
                app_a.window.invoke_create_network("Rede de teste".into());
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
        code.to_string()
    };
    anyhow::ensure!(
        invite_code.starts_with("NEXO1."),
        "código de convite com formato inesperado: {invite_code}"
    );

    // 2) A envia uma mensagem ANTES de B se conectar: B precisa buscá-la via
    //    sincronização quando entrar (conexão tardia com histórico).
    let sent = app_a.window.invoke_send_message("ola do marco 1".into());
    anyhow::ensure!(sent, "instância A não conseguiu enviar a mensagem");

    // 3) B entra na comunidade existente pelo convite (UI de convite, lado B).
    app_b.window.invoke_join_network(invite_code.into());
    wait_for(
        "instância B aceitar o convite",
        Duration::from_secs(30),
        || app_b.window.get_has_community(),
        || format!("B status={}", app_b.window.get_status_text()),
    )
    .await?;

    // 4.1) Mensagem direta: a UI deve listar o outro membro e o envio deve
    // atravessar o sinal autenticado, aparecer no outro app e persistir.
    wait_for(
        "A listar o membro para mensagem direta",
        Duration::from_secs(60),
        || app_a.window.get_direct_peers().row_count() > 0,
        || {
            format!(
                "A direct-peers={}",
                app_a.window.get_direct_peers().row_count()
            )
        },
    )
    .await?;
    let recipient = app_a
        .window
        .get_direct_peers()
        .row_data(0)
        .context("destinatário da mensagem direta")?
        .id
        .clone();
    app_a.window.invoke_select_direct_peer(recipient);
    wait_for(
        "B listar o membro para mensagem direta",
        Duration::from_secs(60),
        || app_b.window.get_direct_peers().row_count() > 0,
        || {
            format!(
                "B direct-peers={}",
                app_b.window.get_direct_peers().row_count()
            )
        },
    )
    .await?;
    let sender = app_b
        .window
        .get_direct_peers()
        .row_data(0)
        .context("remetente da mensagem direta")?
        .id
        .clone();
    app_b.window.invoke_select_direct_peer(sender);
    let sent_direct = app_a
        .window
        .invoke_send_direct_message("dm do marco 1".into());
    anyhow::ensure!(sent_direct, "instância A não enfileirou a mensagem direta");
    wait_for(
        "B receber a mensagem direta",
        Duration::from_secs(90),
        || direct_messages_contain(&app_b.window.get_direct_messages(), "dm do marco 1"),
        || {
            format!(
                "A status={} | B status={} | B direct-msgs={}",
                app_a.window.get_status_text(),
                app_b.window.get_status_text(),
                app_b.window.get_direct_messages().row_count()
            )
        },
    )
    .await?;

    // 4.2) B sai da rede. A ainda deve conseguir criar a mensagem, persistindo
    // o envelope cifrado para a próxima sincronização.
    app_b.shutdown();
    drop(app_b);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sent_offline = app_a
        .window
        .invoke_send_direct_message("dm offline do marco 1".into());
    anyhow::ensure!(sent_offline, "instância A não enfileirou a DM offline");
    wait_for(
        "A persistir a DM offline",
        Duration::from_secs(30),
        || direct_messages_contain(&app_a.window.get_direct_messages(), "dm offline do marco 1"),
        || format!("A status={}", app_a.window.get_status_text()),
    )
    .await?;

    // B retorna usando a mesma identidade/banco e deve receber o envelope pela
    // página de sincronização, sem que A reenvie o texto em claro.
    let mut app_b = nexo_app::start_app(&dir_b).context("reiniciar instância B")?;
    wait_for(
        "B reconectar à comunidade após reinício",
        Duration::from_secs(60),
        || app_b.window.get_direct_peers().row_count() > 0,
        || format!("B status={}", app_b.window.get_status_text()),
    )
    .await?;
    let sender = app_b
        .window
        .get_direct_peers()
        .row_data(0)
        .context("remetente da DM offline")?
        .id
        .clone();
    app_b.window.invoke_select_direct_peer(sender);
    wait_for(
        "B sincronizar a DM offline",
        Duration::from_secs(90),
        || direct_messages_contain(&app_b.window.get_direct_messages(), "dm offline do marco 1"),
        || {
            format!(
                "A status={} | B status={} | B direct-msgs={}",
                app_a.window.get_status_text(),
                app_b.window.get_status_text(),
                app_b.window.get_direct_messages().row_count()
            )
        },
    )
    .await?;

    // 4) B recebe o histórico da comunidade criada antes da conexão.
    wait_for(
        "instância B receber a mensagem via conexão tardia",
        Duration::from_secs(90),
        || messages_contain(&app_b.window.get_messages(), "ola do marco 1"),
        || {
            format!(
                "A status={} | B status={} msgs={}",
                app_a.window.get_status_text(),
                app_b.window.get_status_text(),
                app_b.window.get_messages().row_count()
            )
        },
    )
    .await?;

    // 5) Catálogo de áudio e vídeo: A seleciona o primeiro dispositivo de entrada e vídeo; o
    //    rótulo refletido deve corresponder ao nome dele (mapeamento nome->id).
    let input_names = app_a.window.get_input_device_names();
    if input_names.row_count() > 0 {
        let first = input_names
            .row_data(0)
            .context("primeiro dispositivo de entrada")?
            .to_string();
        app_a
            .window
            .invoke_select_input_device(first.clone().into());
        anyhow::ensure!(
            app_a.window.get_selected_input_device().as_str() == first,
            "rótulo do microfone deveria refletir o dispositivo selecionado"
        );
    }
    let video_names = app_a.window.get_video_device_names();
    if video_names.row_count() > 0 {
        let first_cam = video_names
            .row_data(0)
            .context("primeiro dispositivo de vídeo")?
            .to_string();
        app_a
            .window
            .invoke_select_video_device(first_cam.clone().into());
        anyhow::ensure!(
            app_a.window.get_selected_video_device().as_str() == first_cam,
            "rótulo da câmera deveria refletir o dispositivo selecionado"
        );
    }

    // 6) Controles de chamada: A entra na voz primeiro.
    app_a.window.invoke_join_call();
    wait_for(
        "A iniciar a chamada",
        Duration::from_secs(30),
        || app_a.window.get_call_active(),
        || {
            format!(
                "A call-active={} starting={} status={}",
                app_a.window.get_call_active(),
                app_a.window.get_call_starting(),
                app_a.window.get_call_status()
            )
        },
    )
    .await?;
    wait_for(
        "motor de voz de A pronto",
        Duration::from_secs(30),
        || {
            app_a
                .window
                .get_call_status()
                .as_str()
                .contains("Na voz, aguardando pessoas")
        },
        || format!("A call-status={}", app_a.window.get_call_status()),
    )
    .await?;

    // The call controls must update immediately while the command is being
    // processed by the media thread; this prevents a slow device from making
    // a button appear unresponsive.
    anyhow::ensure!(
        app_a.window.get_video_enabled(),
        "camera should start enabled"
    );
    app_a.window.invoke_toggle_video();
    anyhow::ensure!(
        !app_a.window.get_video_enabled(),
        "camera button should disable video immediately"
    );
    app_a.window.invoke_toggle_video();
    anyhow::ensure!(
        app_a.window.get_video_enabled(),
        "camera button should re-enable video immediately"
    );

    // 6) B entra na voz; a negociação WebRTC real deve conectar os dois.
    app_b.window.invoke_join_call();
    wait_for(
        "B iniciar a chamada",
        Duration::from_secs(30),
        || app_b.window.get_call_active(),
        || {
            format!(
                "B call-active={} starting={} status={}",
                app_b.window.get_call_active(),
                app_b.window.get_call_starting(),
                app_b.window.get_call_status()
            )
        },
    )
    .await?;
    wait_for(
        "A enxergar 1 participante conectado via WebRTC",
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
                "A call-status={} | B call-status={}",
                app_a.window.get_call_status(),
                app_b.window.get_call_status()
            )
        },
    )
    .await?;
    wait_for(
        "B enxergar 1 participante conectado via WebRTC",
        Duration::from_secs(90),
        || {
            app_b
                .window
                .get_call_status()
                .as_str()
                .contains("1 pessoa(s) conectada(s)")
        },
        || {
            format!(
                "A call-status={} | B call-status={}",
                app_a.window.get_call_status(),
                app_b.window.get_call_status()
            )
        },
    )
    .await?;

    // 7) Estado de participante na UI: A deve listar B como conectado.
    wait_for(
        "A listar 1 participante conectado na UI",
        Duration::from_secs(30),
        || {
            (0..app_a.window.get_participants().row_count()).any(|index| {
                app_a
                    .window
                    .get_participants()
                    .row_data(index)
                    .is_some_and(|participant| participant.connected)
            })
        },
        || {
            format!(
                "A participants={}",
                app_a.window.get_participants().row_count()
            )
        },
    )
    .await?;

    // 8) Controles de chamada: silenciar e sair da voz; a lista de participantes
    //    precisa esvaziar depois que o motor de voz for encerrado.
    app_a.window.invoke_set_muted(true);
    anyhow::ensure!(app_a.window.get_call_muted(), "A deveria estar mutado");
    app_a.window.invoke_leave_call();
    anyhow::ensure!(
        !app_a.window.get_call_active(),
        "A deveria ter saído da voz"
    );
    anyhow::ensure!(
        app_a.window.get_call_status().as_str() == "Fora da voz",
        "status de voz de A deveria voltar a 'Fora da voz'"
    );
    wait_for(
        "lista de participantes de A esvaziar após sair da voz",
        Duration::from_secs(30),
        || app_a.window.get_participants().row_count() == 0,
        || {
            format!(
                "A participants={}",
                app_a.window.get_participants().row_count()
            )
        },
    )
    .await?;

    // 9) Encerramento limpo: sinaliza shutdown, libera os bancos e remove os
    //    diretórios temporários.
    app_a.shutdown();
    app_b.shutdown();
    drop(app_a);
    drop(app_b);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    slint::quit_event_loop()
        .map_err(|error| anyhow::anyhow!("ao encerrar o event loop: {error}"))?;
    Ok(())
}

#[test]
fn two_instances_share_community_history_and_voice() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let scenario_failed = Arc::new(AtomicBool::new(false));
    let scenario_failed_task = Arc::clone(&scenario_failed);
    slint::spawn_local(Compat::new(async move {
        if let Err(error) = run_scenario().await {
            eprintln!("two-instances scenario failed: {error:#}");
            scenario_failed_task.store(true, Ordering::Release);
            let _ = slint::quit_event_loop();
        }
    }))
    .expect("spawn scenario on the Slint event loop");
    slint::run_event_loop().expect("run Slint event loop");
    assert!(
        !scenario_failed.load(Ordering::Acquire),
        "two-instances scenario failed; see the diagnostic above"
    );
}
