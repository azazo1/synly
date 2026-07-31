use crate::input::InputMode;
use crate::protocol::{CapabilityEpoch, RuntimeCapabilities};
use crate::settings::{AudioMode, ClipboardMode};
use anyhow::Result;

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum CapabilityError {
    StaleGeneration { generation: u64, current: u64 },
    FutureGeneration { generation: u64, current: u64 },
    ConflictingGeneration { generation: u64 },
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration {
                generation,
                current,
            } => write!(
                formatter,
                "capability generation {generation} is stale, current generation is {current}"
            ),
            Self::FutureGeneration {
                generation,
                current,
            } => write!(
                formatter,
                "capability generation {generation} is ahead of local generation {current}"
            ),
            Self::ConflictingGeneration { generation } => write!(
                formatter,
                "capability update generation {generation} conflicts with the current update"
            ),
        }
    }
}

impl std::error::Error for CapabilityError {}

#[derive(Clone, Debug)]
pub struct CapabilityState {
    host_role: bool,
    local_generation: u64,
    remote_generation: u64,
    local: RuntimeCapabilities,
    remote: RuntimeCapabilities,
    acknowledged_local_generation: u64,
    acknowledged_local: RuntimeCapabilities,
}

impl CapabilityState {
    pub fn new(
        host_role: bool,
        local: RuntimeCapabilities,
        remote: RuntimeCapabilities,
    ) -> Self {
        Self {
            host_role,
            local_generation: 0,
            remote_generation: 0,
            local,
            remote,
            acknowledged_local_generation: 0,
            acknowledged_local: local,
        }
    }

    pub fn local_generation(&self) -> u64 {
        self.local_generation
    }

    #[cfg(test)]
    pub fn local(&self) -> RuntimeCapabilities {
        self.local
    }

    pub fn is_local_acknowledged(&self) -> bool {
        self.local_generation == self.acknowledged_local_generation
    }

    pub fn epoch(&self) -> CapabilityEpoch {
        if self.host_role {
            CapabilityEpoch {
                host_generation: self.local_generation,
                client_generation: self.remote_generation,
            }
        } else {
            CapabilityEpoch {
                host_generation: self.remote_generation,
                client_generation: self.local_generation,
            }
        }
    }

    pub fn effective_local(&self) -> RuntimeCapabilities {
        if self.is_local_acknowledged() {
            self.local
        } else {
            intersect_capabilities(self.acknowledged_local, self.local)
        }
    }

    pub fn effective_remote(&self) -> RuntimeCapabilities {
        self.remote
    }

    pub fn set_local(&mut self, capabilities: RuntimeCapabilities) -> Option<(u64, RuntimeCapabilities)> {
        if capabilities == self.local {
            return None;
        }
        self.local_generation = self.local_generation.saturating_add(1);
        self.local = capabilities;
        Some((self.local_generation, capabilities))
    }

    pub fn bump_local(&mut self) -> (u64, RuntimeCapabilities) {
        self.local_generation = self.local_generation.saturating_add(1);
        (self.local_generation, self.local)
    }

    pub fn apply_remote(
        &mut self,
        generation: u64,
        capabilities: RuntimeCapabilities,
    ) -> Result<bool> {
        if generation < self.remote_generation {
            return Err(CapabilityError::StaleGeneration {
                generation,
                current: self.remote_generation,
            }
            .into());
        }
        if generation == self.remote_generation {
            if capabilities != self.remote {
                return Err(CapabilityError::ConflictingGeneration { generation }.into());
            }
            return Ok(false);
        }
        self.remote_generation = generation;
        self.remote = capabilities;
        Ok(true)
    }

    pub fn apply_ack(&mut self, generation: u64) -> Result<bool> {
        if generation < self.acknowledged_local_generation {
            return Ok(false);
        }
        if generation > self.local_generation {
            return Err(CapabilityError::FutureGeneration {
                generation,
                current: self.local_generation,
            }
            .into());
        }
        if generation < self.local_generation {
            return Ok(false);
        }
        self.acknowledged_local_generation = generation;
        self.acknowledged_local = self.local;
        Ok(true)
    }

    pub fn audio_ready(&self) -> bool {
        self.is_local_acknowledged()
    }

    pub fn current_epoch(&self, epoch: CapabilityEpoch) -> bool {
        self.epoch() == epoch && self.is_local_acknowledged()
    }

}

fn intersect_capabilities(
    left: RuntimeCapabilities,
    right: RuntimeCapabilities,
) -> RuntimeCapabilities {
    RuntimeCapabilities {
        clipboard_mode: intersect_clipboard(left.clipboard_mode, right.clipboard_mode),
        audio_mode: intersect_audio(left.audio_mode, right.audio_mode),
        input_mode: intersect_input(left.input_mode, right.input_mode),
    }
}

fn intersect_clipboard(left: ClipboardMode, right: ClipboardMode) -> ClipboardMode {
    let send = left.can_send() && right.can_send();
    let receive = left.can_receive() && right.can_receive();
    match (send, receive) {
        (true, true) => ClipboardMode::Both,
        (true, false) => ClipboardMode::Send,
        (false, true) => ClipboardMode::Receive,
        (false, false) => ClipboardMode::Off,
    }
}

fn intersect_audio(left: AudioMode, right: AudioMode) -> AudioMode {
    if left == right { left } else { AudioMode::Off }
}

fn intersect_input(left: InputMode, right: InputMode) -> InputMode {
    if left == right { left } else { InputMode::Off }
}

#[cfg(test)]
mod tests {
    use super::CapabilityState;
    use crate::input::InputMode;
    use crate::protocol::{CapabilityEpoch, RuntimeCapabilities};
    use crate::settings::{AudioMode, ClipboardMode};

    fn caps(clipboard_mode: ClipboardMode, audio_mode: AudioMode, input_mode: InputMode) -> RuntimeCapabilities {
        RuntimeCapabilities {
            clipboard_mode,
            audio_mode,
            input_mode,
        }
    }

    #[test]
    fn local_enable_waits_for_ack_but_disable_is_immediate() {
        let mut state = CapabilityState::new(
            true,
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
        );
        state.set_local(caps(ClipboardMode::Both, AudioMode::Off, InputMode::Off));
        assert_eq!(state.effective_local().clipboard_mode, ClipboardMode::Off);
        state.apply_ack(1).unwrap();
        assert_eq!(state.effective_local().clipboard_mode, ClipboardMode::Both);

        state.set_local(caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off));
        assert_eq!(state.effective_local().clipboard_mode, ClipboardMode::Off);
    }

    #[test]
    fn epoch_tracks_host_and_client_generations() {
        let mut host = CapabilityState::new(
            true,
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
        );
        let mut client = CapabilityState::new(
            false,
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
        );
        host.set_local(caps(ClipboardMode::Send, AudioMode::Off, InputMode::Off));
        client.apply_remote(1, host.local()).unwrap();
        client.set_local(caps(ClipboardMode::Receive, AudioMode::Off, InputMode::Off));
        host.apply_remote(1, client.local()).unwrap();
        assert_eq!(host.epoch(), CapabilityEpoch { host_generation: 1, client_generation: 1 });
        assert_eq!(client.epoch(), host.epoch());
    }

    #[test]
    fn stale_remote_update_is_rejected() {
        let mut state = CapabilityState::new(
            true,
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
            caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off),
        );
        state.apply_remote(2, caps(ClipboardMode::Send, AudioMode::Off, InputMode::Off)).unwrap();
        assert!(state.apply_remote(1, caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off)).is_err());
    }

    #[test]
    fn concurrent_generations_converge_to_the_same_epoch() {
        let off = caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off);
        let mut host = CapabilityState::new(true, off, off);
        let mut client = CapabilityState::new(false, off, off);

        let (host_generation, host_caps) =
            host.set_local(caps(ClipboardMode::Send, AudioMode::Off, InputMode::Off)).unwrap();
        let (client_generation, client_caps) = client
            .set_local(caps(ClipboardMode::Receive, AudioMode::Off, InputMode::Off))
            .unwrap();

        host.apply_remote(client_generation, client_caps).unwrap();
        client.apply_remote(host_generation, host_caps).unwrap();
        host.apply_ack(host_generation).unwrap();
        client.apply_ack(client_generation).unwrap();

        assert_eq!(host.epoch(), client.epoch());
        assert!(host.current_epoch(host.epoch()));
        assert!(client.current_epoch(client.epoch()));
    }

    #[test]
    fn stale_ack_does_not_enable_new_generation() {
        let off = caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off);
        let mut state = CapabilityState::new(true, off, off);
        state
            .set_local(caps(ClipboardMode::Send, AudioMode::Off, InputMode::Off))
            .unwrap();
        state
            .set_local(caps(ClipboardMode::Both, AudioMode::Off, InputMode::Off))
            .unwrap();

        assert!(!state.apply_ack(1).unwrap());
        assert!(!state.is_local_acknowledged());
        assert_eq!(state.effective_local().clipboard_mode, ClipboardMode::Off);
        assert!(state.apply_ack(2).unwrap());
        assert_eq!(state.effective_local().clipboard_mode, ClipboardMode::Both);
    }

    #[test]
    fn conflicting_replay_is_rejected() {
        let off = caps(ClipboardMode::Off, AudioMode::Off, InputMode::Off);
        let mut state = CapabilityState::new(true, off, off);
        state
            .apply_remote(1, caps(ClipboardMode::Send, AudioMode::Off, InputMode::Off))
            .unwrap();

        assert!(
            state
                .apply_remote(1, caps(ClipboardMode::Receive, AudioMode::Off, InputMode::Off))
                .is_err()
        );
    }
}
