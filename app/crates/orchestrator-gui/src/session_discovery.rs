use orchestrator_host::session::CliKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreshDiscovery {
    Continue { seen: bool },
    Stop,
}

pub(crate) fn fresh_session_discovery_next(
    info: Option<(bool, bool)>,
    seen: bool,
) -> FreshDiscovery {
    match info {
        Some((alive, has_cli_id)) => {
            if !alive || has_cli_id {
                FreshDiscovery::Stop
            } else {
                FreshDiscovery::Continue { seen: true }
            }
        }
        None if seen => FreshDiscovery::Stop,
        None => FreshDiscovery::Continue { seen },
    }
}

pub(crate) fn restore_row_kind(row_kind: &str, has_codex_rollout: bool) -> CliKind {
    match row_kind.trim().to_ascii_lowercase().as_str() {
        "codex" => CliKind::Codex,
        "shell" => CliKind::Shell,
        _ if has_codex_rollout => CliKind::Codex,
        _ => CliKind::Claude,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_polling_live_session_without_cli_id() {
        assert_eq!(
            fresh_session_discovery_next(Some((true, false)), false),
            FreshDiscovery::Continue { seen: true }
        );
    }

    #[test]
    fn stops_after_id_death_or_disappearance() {
        assert_eq!(
            fresh_session_discovery_next(Some((true, true)), true),
            FreshDiscovery::Stop
        );
        assert_eq!(
            fresh_session_discovery_next(Some((false, false)), true),
            FreshDiscovery::Stop
        );
        assert_eq!(
            fresh_session_discovery_next(None, true),
            FreshDiscovery::Stop
        );
        assert_eq!(
            fresh_session_discovery_next(None, false),
            FreshDiscovery::Continue { seen: false }
        );
    }

    #[test]
    fn restore_kind_infers_codex_from_rollout_even_for_legacy_rows() {
        assert_eq!(restore_row_kind("codex", false), CliKind::Codex);
        assert_eq!(restore_row_kind("claude", true), CliKind::Codex);
        assert_eq!(restore_row_kind("claude", false), CliKind::Claude);
    }
}
