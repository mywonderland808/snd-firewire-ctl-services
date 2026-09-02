// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Marco

//! Mirror SHIFT+USER remote mute for mixer channels past the 12-slot ``mixer-input-mute`` array.

use {super::*, protocols::tcelectronic::studio::*, tracing::warn};

/// Last mixer channel index reachable via ``mixer-input-mute`` (channel-indexed layout).
pub const USER_REMOTE_MUTE_MAX_CHANNEL: usize = 11;

/// Gain at or below this level is treated as remote-ducked (matches k48-mixer UI).
pub const REMOTE_GAIN_DUCK_THRESHOLD: i32 = -400;

pub const MIXER_INPUT_GAIN_MIN: i32 = -1000;

/// Mixer monitor channels exposed as ``mixer-input-gain`` (``src_pairs`` × 2).
pub const MIXER_MONITOR_CHANNEL_COUNT: usize = STUDIO_MIXER_SRC_PAIR_COUNT * 2;

/// Cached pre-remote-mute gains and helpers for ALSA gain overlay.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct RemoteUserMuteMirror {
    saved_gains: [Option<i32>; MIXER_MONITOR_CHANNEL_COUNT],
}

#[cfg(test)]
impl RemoteUserMuteMirror {
    pub fn saved_gain(&self, ch: usize) -> Option<i32> {
        self.saved_gains[ch]
    }
}

impl RemoteUserMuteMirror {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Map each mixer channel (0-23) to the USER button index (0-5) that targets it.
    pub fn user_button_for_channel(
        remote: &StudioRemote,
        mixer: &StudioMixerState,
    ) -> [Option<usize>; MIXER_MONITOR_CHANNEL_COUNT] {
        let mut map = [None; MIXER_MONITOR_CHANNEL_COUNT];
        for (user_i, assign) in remote.user_assigns.iter().enumerate() {
            if *assign == SrcEntry::Unused {
                continue;
            }
            for (ch, param) in mixer_monitor_src_params(mixer).enumerate() {
                if param.src == *assign {
                    map[ch] = Some(user_i);
                }
            }
        }
        map
    }

    fn channel_remote_muted(
        ch: usize,
        mixer: &StudioMixerState,
        meter: &StudioRemoteMeter,
        user_map: &[Option<usize>; MIXER_MONITOR_CHANNEL_COUNT],
    ) -> bool {
        if ch <= USER_REMOTE_MUTE_MAX_CHANNEL {
            return false;
        }
        let high = ch - (USER_REMOTE_MUTE_MAX_CHANNEL + 1);
        if mixer.mutes_high[high] {
            return true;
        }
        if let Some(user_i) = user_map[ch] {
            if meter.user_mutes[user_i] {
                return true;
            }
            let gain = mixer_monitor_src_params(mixer)
                .nth(ch)
                .map(|p| p.gain_to_main)
                .unwrap_or(0);
            if gain <= REMOTE_GAIN_DUCK_THRESHOLD {
                return true;
            }
        }
        false
    }

    fn is_duck_write(val: i32) -> bool {
        val <= REMOTE_GAIN_DUCK_THRESHOLD
    }

    /// Build ALSA ``mixer-input-gain`` values with remote USER duck overlay for ch >= 12.
    pub fn overlay_mixer_input_gain(
        &mut self,
        mixer: &StudioMixerState,
        remote: &StudioRemote,
        meter: &StudioRemoteMeter,
    ) -> Vec<i32> {
        let user_map = Self::user_button_for_channel(remote, mixer);
        let mut vals: Vec<i32> = mixer_monitor_src_params(mixer)
            .map(|param| param.gain_to_main)
            .collect();

        for ch in (USER_REMOTE_MUTE_MAX_CHANNEL + 1)..vals.len() {
            if Self::channel_remote_muted(ch, mixer, meter, &user_map) {
                if self.saved_gains[ch].is_none() && vals[ch] > REMOTE_GAIN_DUCK_THRESHOLD {
                    self.saved_gains[ch] = Some(vals[ch]);
                }
                vals[ch] = MIXER_INPUT_GAIN_MIN;
            } else if let Some(saved) = self.saved_gains[ch].take() {
                if vals[ch] <= REMOTE_GAIN_DUCK_THRESHOLD {
                    vals[ch] = saved;
                }
            }
        }

        vals
    }

    /// Apply userspace ``mixer-input-gain`` write, updating firmware params and shadow gains.
    pub fn apply_mixer_input_gain_write(
        &mut self,
        params: &mut StudioMixerState,
        written: &[i32],
    ) {
        if written.len() > MIXER_MONITOR_CHANNEL_COUNT {
            warn!(
                "mixer-input-gain write has {} values; using first {}",
                written.len(),
                MIXER_MONITOR_CHANNEL_COUNT
            );
        }
        let n = written.len().min(MIXER_MONITOR_CHANNEL_COUNT);
        for (ch, &val) in written.iter().take(n).enumerate() {
            if ch <= USER_REMOTE_MUTE_MAX_CHANNEL {
                set_gain_to_main(params, ch, val);
                continue;
            }

            let high = ch - (USER_REMOTE_MUTE_MAX_CHANNEL + 1);
            if high >= params.mutes_high.len() {
                continue;
            }
            if Self::is_duck_write(val) {
                let current = gain_to_main(params, ch);
                if self.saved_gains[ch].is_none() && current > REMOTE_GAIN_DUCK_THRESHOLD {
                    self.saved_gains[ch] = Some(current);
                }
                set_gain_to_main(params, ch, MIXER_INPUT_GAIN_MIN);
                params.mutes_high[high] = true;
            } else {
                let restore = self.saved_gains[ch].take().unwrap_or(val);
                set_gain_to_main(params, ch, restore);
                params.mutes_high[high] = false;
            }
        }
    }
}

fn mixer_monitor_src_params(state: &StudioMixerState) -> impl Iterator<Item = &MonitorSrcParam> {
    state
        .src_pairs
        .iter()
        .flat_map(|pair| pair.params.iter())
}

fn gain_to_main(state: &StudioMixerState, ch: usize) -> i32 {
    if ch >= MIXER_MONITOR_CHANNEL_COUNT {
        return 0;
    }
    let pair = ch / 2;
    let slot = ch % 2;
    state.src_pairs[pair].params[slot].gain_to_main
}

fn set_gain_to_main(state: &mut StudioMixerState, ch: usize, val: i32) {
    if ch >= MIXER_MONITOR_CHANNEL_COUNT {
        return;
    }
    let pair = ch / 2;
    let slot = ch % 2;
    state.src_pairs[pair].params[slot].gain_to_main = val;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixer_with_source_at(ch: usize, src: SrcEntry) -> StudioMixerState {
        let mut mixer = StudioMixerState::default();
        let pair = ch / 2;
        let slot = ch % 2;
        mixer.src_pairs[pair].params[slot].src = src;
        mixer.src_pairs[pair].params[slot].gain_to_main = -200;
        mixer
    }

    #[test]
    fn user_button_maps_patch_slot() {
        let mut remote = StudioRemote::default();
        remote.user_assigns[2] = SrcEntry::Analog(8); // Analog-9

        let mixer = mixer_with_source_at(12, SrcEntry::Analog(8));

        let map = RemoteUserMuteMirror::user_button_for_channel(&remote, &mixer);
        assert_eq!(map[12], Some(2));
        assert!(map[11].is_none());
    }

    #[test]
    fn mutes_high_ducks_alsa_gain() {
        let remote = StudioRemote::default();
        let meter = StudioRemoteMeter::default();
        let mut mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        mixer.mutes_high[0] = true;

        let mut mirror = RemoteUserMuteMirror::default();
        let vals = mirror.overlay_mixer_input_gain(&mixer, &remote, &meter);
        assert_eq!(vals[12], MIXER_INPUT_GAIN_MIN);
        assert_eq!(mirror.saved_gains[12], Some(-200));
    }

    #[test]
    fn user_meter_latch_ducks_assigned_channel() {
        let mut remote = StudioRemote::default();
        remote.user_assigns[2] = SrcEntry::Analog(8);

        let mut meter = StudioRemoteMeter::default();
        meter.user_mutes[2] = true;

        let mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        let mut mirror = RemoteUserMuteMirror::default();
        let vals = mirror.overlay_mixer_input_gain(&mixer, &remote, &meter);
        assert_eq!(vals[12], MIXER_INPUT_GAIN_MIN);
    }

    #[test]
    fn unmute_restores_saved_gain() {
        let remote = StudioRemote::default();
        let meter = StudioRemoteMeter::default();
        let mut mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        mixer.mutes_high[0] = false;
        mixer.src_pairs[6].params[0].gain_to_main = MIXER_INPUT_GAIN_MIN;

        let mut mirror = RemoteUserMuteMirror {
            saved_gains: {
                let mut s = [None; 24];
                s[12] = Some(-200);
                s
            },
        };
        let vals = mirror.overlay_mixer_input_gain(&mixer, &remote, &meter);
        assert_eq!(vals[12], -200);
        assert!(mirror.saved_gains[12].is_none());
    }

    #[test]
    fn write_duck_sets_mutes_high() {
        let mut mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        let mut mirror = RemoteUserMuteMirror::default();
        let mut written = [-200i32; 24];
        written[12] = MIXER_INPUT_GAIN_MIN;
        mirror.apply_mixer_input_gain_write(&mut mixer, &written);
        assert!(mixer.mutes_high[0]);
        assert_eq!(mixer.src_pairs[6].params[0].gain_to_main, MIXER_INPUT_GAIN_MIN);
        assert_eq!(mirror.saved_gains[12], Some(-200));
    }

    #[test]
    fn write_duck_at_remote_threshold() {
        let mut mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        let mut mirror = RemoteUserMuteMirror::default();
        let mut written = [-200i32; 24];
        written[12] = REMOTE_GAIN_DUCK_THRESHOLD;
        mirror.apply_mixer_input_gain_write(&mut mixer, &written);
        assert!(mixer.mutes_high[0]);
        assert_eq!(mixer.src_pairs[6].params[0].gain_to_main, MIXER_INPUT_GAIN_MIN);
    }

    #[test]
    fn write_unmute_restores_saved_gain() {
        let mut mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        mixer.mutes_high[0] = true;
        mixer.src_pairs[6].params[0].gain_to_main = MIXER_INPUT_GAIN_MIN;
        let mut mirror = RemoteUserMuteMirror {
            saved_gains: {
                let mut s = [None; 24];
                s[12] = Some(-200);
                s
            },
        };
        let mut written = [-1000i32; 24];
        written[12] = 0;
        mirror.apply_mixer_input_gain_write(&mut mixer, &written);
        assert!(!mixer.mutes_high[0]);
        assert_eq!(mixer.src_pairs[6].params[0].gain_to_main, -200);
        assert!(mirror.saved_gains[12].is_none());
    }

    #[test]
    fn write_ignores_trailing_extra_values() {
        let mut mixer = mixer_with_source_at(12, SrcEntry::Analog(8));
        let mut mirror = RemoteUserMuteMirror::default();
        let mut written = vec![-200i32; 25];
        written[12] = MIXER_INPUT_GAIN_MIN;
        mirror.apply_mixer_input_gain_write(&mut mixer, &written);
        assert!(mixer.mutes_high[0]);
    }
}
