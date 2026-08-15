//! System Tray controller for background presence and quick call actions.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayAction {
    RestoreWindow,
    ToggleMute,
    DisconnectCall,
    Quit,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrayState {
    pub is_minimized: bool,
    pub is_muted: bool,
    pub in_call: bool,
    pub active_community_name: Option<String>,
}

impl TrayState {
    #[must_use]
    pub fn tooltip_text(&self) -> String {
        if self.in_call {
            let mute_status = if self.is_muted { " (Mutado)" } else { "" };
            format!("Nexo - Em chamada{mute_status}")
        } else if let Some(ref comm) = self.active_community_name {
            format!("Nexo - {comm}")
        } else {
            "Nexo - Conectado na rede local".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_state_formats_tooltip_according_to_call_status() {
        let mut state = TrayState::default();
        assert_eq!(state.tooltip_text(), "Nexo - Conectado na rede local");

        state.active_community_name = Some("Rust Devs".into());
        assert_eq!(state.tooltip_text(), "Nexo - Rust Devs");

        state.in_call = true;
        state.is_muted = false;
        assert_eq!(state.tooltip_text(), "Nexo - Em chamada");

        state.is_muted = true;
        assert_eq!(state.tooltip_text(), "Nexo - Em chamada (Mutado)");
    }
}
