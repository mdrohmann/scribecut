//! Voice cleanup for recorded narration.
//!
//! A built-in microphone in a room gives audio that is quiet, uneven and sitting
//! on low-frequency rumble. The chain here is built from measurements of real
//! scribecut recordings rather than a generic "podcast preset", and each stage
//! earns its place:
//!
//! * **High-pass.** Measured per-band signal-to-noise of a real recording was
//!   only +17 dB between 30-60 Hz and +21 dB at 60-120 Hz, against +39 to +50 dB
//!   everywhere above 250 Hz. Speech needs almost nothing below 85 Hz, so
//!   rolling it off removes nearly all of the noise and none of the voice.
//! * **No denoiser by default.** Above 250 Hz those same recordings were already
//!   39-50 dB clean, where `afftdn` would cost more in artefacts than it buys.
//!   [`Preset::Strong`] adds it for genuinely noisy rooms.
//! * **Speech normaliser.** The recordings came in at -32 to -38 LUFS, some 20 dB
//!   under the usual speech target, so they need a lot of gain. `speechnorm`
//!   only lifts what is above its threshold, so pauses stay put: measured, the
//!   voice came up ~12 dB while the room tone moved less than 1 dB, taking
//!   signal-to-noise from 36 dB to 47 dB. Plain gain, upward compression or
//!   `dynaudnorm` all lifted the room tone by 29-37 dB instead, which is the
//!   noise swell in every pause that makes cheap cleanup sound worse than none.
//! * **Compressor, downward only.** `makeup=1` is unity: this stage can lower a
//!   loud passage but can never raise the noise floor.
//! * **Loudness normalisation.** Does the final gain and, importantly, the
//!   true-peak ceiling. `alimiter` caps *sample* peaks and measured a consistent
//!   +1.3 dB inter-sample overshoot (clipping at +0.3 dBFS); `loudnorm` is
//!   true-peak aware and landed on exactly -1.5 dBTP on every file tested.
//!
//! The same chain is used for the export, for the waveform and for
//! transcription: on a real recording it took whisper from 128 recognised words
//! to 188, so this is not only about how the result sounds.

/// How much cleanup to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// Leave the audio exactly as recorded.
    Off,
    /// Rumble, leveling and loudness. The default.
    Voice,
    /// Also denoise and de-ess, for a noisy room or a hissy microphone.
    Strong,
}

impl Preset {
    /// Reads `SCRIBECUT_AUDIO`; anything unrecognised means the default.
    pub fn from_env() -> Self {
        match std::env::var("SCRIBECUT_AUDIO").unwrap_or_default().trim().to_lowercase().as_str() {
            "off" | "none" | "raw" => Self::Off,
            "strong" | "noisy" => Self::Strong,
            _ => Self::Voice,
        }
    }
}

/// Programme loudness aimed for, in LUFS. -16 is the usual target for speech;
/// override with `SCRIBECUT_LOUDNESS`.
fn target_lufs() -> f64 {
    std::env::var("SCRIBECUT_LOUDNESS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| (-40.0..=-5.0).contains(v))
        .unwrap_or(-16.0)
}

/// Single-pass `loudnorm` has to decide its gain as it goes, and measured
/// consistently about 2 LU below whatever it was asked for. Asking for a little
/// more lands on the real target, and avoids a second measuring pass over the
/// whole programme.
const SINGLE_PASS_BIAS: f64 = 2.0;

/// Highest true peak the loudness stage will allow, in dBFS.
///
/// Deliberately below the -1 dBTP that EBU R128 asks for, because the export
/// re-encodes to AAC afterwards and a lossy codec does not preserve peaks:
/// measured, AAC pushed the true peak back up by 0.7 to 1.3 dB. Asking for
/// -2.5 here lands the finished file near -1.8 dBTP.
const TRUE_PEAK: f64 = -2.5;

/// Filter names this ffmpeg build offers.
///
/// `speechnorm` and `deesser` only arrived in ffmpeg 4.3, and `afftdn` in 4.1.
/// Every current distribution is well past that, but an older build should lose
/// a stage rather than fail the whole export with "No such filter", so the
/// optional stages are checked once and dropped if missing.
fn available_filters() -> &'static std::collections::HashSet<String> {
    static FILTERS: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(|| {
            let Ok(out) = crate::media::ffmpeg().args(["-hide_banner", "-filters"]).output() else {
                return std::collections::HashSet::new();
            };
            // Each line is `  TSC name  in->out  description`.
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.split_whitespace().nth(1))
                .map(str::to_owned)
                .collect()
        });
    &FILTERS
}

/// Whether a stage can be used. An empty probe (no ffmpeg yet) assumes yes, so
/// the chain stays complete and ffmpeg reports the real problem.
fn have(filter: &str) -> bool {
    let filters = available_filters();
    filters.is_empty() || filters.contains(filter)
}

/// The ffmpeg filter chain, or `None` to leave the audio alone.
///
/// Suitable both as an `-af` argument and as a link in a `-filter_complex`
/// graph, since it is a plain comma-separated chain.
pub fn cleanup() -> Option<String> {
    chain(Preset::from_env(), target_lufs())
}

/// The chain for an explicit preset and loudness target, so it can be tested
/// without the environment.
pub fn chain(preset: Preset, target: f64) -> Option<String> {
    if preset == Preset::Off {
        return None;
    }
    let mut stages: Vec<String> = Vec::new();

    // Two second-order sections: one is too gentle to clear the rumble that
    // sits just below the voice.
    stages.push("highpass=f=85:poles=2".into());
    stages.push("highpass=f=85:poles=2".into());

    if preset == Preset::Strong && have("afftdn") {
        // Track the noise rather than assume a fixed floor, and stay gentle:
        // over-denoised speech sounds underwater.
        stages.push("afftdn=nr=12:nf=-50:tn=1".into());
    }

    // `t` is what keeps the pauses quiet; without a threshold this stage would
    // expand room tone up with the voice.
    if have("speechnorm") {
        stages.push("speechnorm=e=20:c=3:t=0.002:p=0.8:r=0.0003:f=0.0003".into());
    }

    // Unity makeup: downward compression only, so the noise floor cannot rise.
    stages.push("acompressor=threshold=-18dB:ratio=3:attack=10:release=250:makeup=1:knee=6".into());

    if preset == Preset::Strong && have("deesser") {
        stages.push("deesser=i=0.2:m=0.5:f=0.5".into());
    }

    // Final gain and the true-peak ceiling. The LRA target is deliberately
    // modest: the dynamics are already handled above, and this stage is here
    // for level, not for squashing.
    stages.push(format!(
        "loudnorm=I={:.1}:TP={:.1}:LRA=11",
        target + SINGLE_PASS_BIAS,
        TRUE_PEAK
    ));

    Some(stages.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_means_untouched() {
        assert!(chain(Preset::Off, -16.0).is_none());
    }

    #[test]
    fn voice_chain_has_the_stages_that_matter() {
        let c = chain(Preset::Voice, -16.0).unwrap();
        // Rumble first, so nothing downstream wastes its range on it.
        assert!(c.starts_with("highpass=f=85:poles=2,highpass=f=85:poles=2,"));
        assert!(c.contains("speechnorm="), "needs ffmpeg 4.3+: {c}");
        // Downward compression only: makeup must stay at unity.
        assert!(c.contains("makeup=1"));
        // Loudness last, and it owns the true-peak ceiling.
        assert!(c.ends_with("loudnorm=I=-14.0:TP=-2.5:LRA=11"));
        // The gentler preset must not denoise or de-ess.
        assert!(!c.contains("afftdn"), "{c}");
        assert!(!c.contains("deesser"), "{c}");
    }

    #[test]
    fn strong_chain_adds_denoise_and_deesser() {
        let c = chain(Preset::Strong, -16.0).unwrap();
        assert!(c.contains("afftdn=nr=12:nf=-50:tn=1"));
        assert!(c.contains("deesser="));
        // Denoising belongs after the high-pass and before the leveller, so it
        // never sees the rumble and the leveller never amplifies its artefacts.
        let (hp, dn) = (c.find("highpass").unwrap(), c.find("afftdn").unwrap());
        let sn = c.find("speechnorm").unwrap();
        assert!(hp < dn && dn < sn, "{c}");
    }

    #[test]
    fn loudness_target_is_biased_and_clamped() {
        // The stated target is what the result should measure, so the filter is
        // asked for `SINGLE_PASS_BIAS` more.
        assert!(chain(Preset::Voice, -23.0).unwrap().ends_with("loudnorm=I=-21.0:TP=-2.5:LRA=11"));

        // Nonsense in the environment must not produce a nonsense chain.
        for bad in ["", "loud", "0", "-100", "12"] {
            // SAFETY: single-threaded test, and the value is read straight back.
            unsafe { std::env::set_var("SCRIBECUT_LOUDNESS", bad) };
            assert_eq!(target_lufs(), -16.0, "{bad:?} should fall back to the default");
        }
        unsafe { std::env::set_var("SCRIBECUT_LOUDNESS", "-20") };
        assert_eq!(target_lufs(), -20.0);
        unsafe { std::env::remove_var("SCRIBECUT_LOUDNESS") };
    }

    #[test]
    fn preset_names() {
        for (value, want) in [
            ("off", Preset::Off),
            ("RAW", Preset::Off),
            ("strong", Preset::Strong),
            ("noisy", Preset::Strong),
            ("voice", Preset::Voice),
            ("nonsense", Preset::Voice),
            ("", Preset::Voice),
        ] {
            // SAFETY: single-threaded test, and the value is read straight back.
            unsafe { std::env::set_var("SCRIBECUT_AUDIO", value) };
            assert_eq!(Preset::from_env(), want, "{value:?}");
        }
        unsafe { std::env::remove_var("SCRIBECUT_AUDIO") };
    }

    /// The stages that need a recent ffmpeg are optional, but the two that do
    /// the heavy lifting -- rolling off rumble and setting the level -- are old
    /// enough to rely on, so a chain is never useless.
    #[test]
    fn essential_stages_are_always_present() {
        for preset in [Preset::Voice, Preset::Strong] {
            let c = chain(preset, -16.0).unwrap();
            assert!(c.contains("highpass=f=85"), "{c}");
            assert!(c.contains("loudnorm="), "{c}");
            assert!(c.contains("acompressor="), "{c}");
            // Never a trailing or doubled separator, whatever was dropped.
            assert!(!c.contains(",,") && !c.ends_with(','), "{c}");
        }
        // This build is modern, so nothing should actually have been dropped.
        assert!(have("speechnorm"), "ffmpeg 4.3+ expected for the full chain");
    }

    /// Runs the chain over a synthetic recording that has everything a bad
    /// built-in microphone gives: far too quiet, low-frequency rumble, hiss,
    /// and long pauses. Checks the measurable claims the chain makes.
    #[test]
    #[ignore]
    fn cleans_up_a_realistically_bad_recording() {
        let dir = std::env::temp_dir().join(format!("scribecut-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("raw.wav");
        // "Speech" as bursts of a voice-like tone with gaps, 26 dB down, over
        // 50 Hz rumble and broadband hiss.
        let source = "sine=frequency=220:duration=12,\
             volume='if(lt(mod(t,3),1.6),1,0)':eval=frame,\
             volume=-26dB[v];\
             sine=frequency=50:duration=12,volume=-30dB[r];\
             anoisesrc=color=white:amplitude=0.004:duration=12[n];\
             [v][r][n]amix=inputs=3:duration=longest:normalize=0";
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-filter_complex", source])
            .args(["-ar", "48000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&raw)
            .status()
            .unwrap()
            .success();
        assert!(ok, "could not synthesise the test recording");

        let clean = dir.join("clean.wav");
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-i"])
            .arg(&raw)
            .args(["-af", &chain(Preset::Voice, -16.0).unwrap()])
            .args(["-ar", "48000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&clean)
            .status()
            .unwrap()
            .success();
        assert!(ok, "the cleanup chain did not run");

        let before = measure(&raw);
        let after = measure(&clean);

        // Loud enough to actually listen to, and near the target.
        assert!(before.lufs < -28.0, "test signal should start quiet: {before:?}");
        assert!((-19.0..=-13.0).contains(&after.lufs), "loudness off target: {after:?}");
        // Never clipping: this is the claim `alimiter` could not keep.
        assert!(after.true_peak <= -1.0, "true peak too hot: {after:?}");
        // The rumble must be gone, not merely quieter in proportion.
        let low_gain = after.low - before.low;
        let band_gain = after.band - before.band;
        assert!(
            low_gain < band_gain - 12.0,
            "high-pass did not clear the rumble: low {low_gain:+.1} dB vs voice band {band_gain:+.1} dB"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug)]
    struct Measured {
        lufs: f64,
        true_peak: f64,
        /// Energy below 120 Hz (rumble) and in the 200-1000 Hz voice band.
        low: f64,
        band: f64,
    }

    #[cfg(test)]
    fn measure(path: &std::path::Path) -> Measured {
        let run = |filter: &str| -> String {
            let out = crate::media::ffmpeg()
                .args(["-hide_banner", "-nostats", "-i"])
                .arg(path)
                .args(["-af", filter, "-f", "null", "-"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        // ffmpeg prefixes some lines with `[Parsed_astats_0 @ 0x..]`, so look
        // for the label anywhere in the line rather than at its start.
        let grab = |text: &str, label: &str| -> f64 {
            text.lines()
                .find_map(|l| l.find(label).map(|i| &l[i + label.len()..]))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(f64::NAN)
        };
        let loudness = run("ebur128=peak=true:framelog=quiet");
        let rms = |filter: &str| {
            grab(&run(&format!("{filter}astats=measure_overall=RMS_level:measure_perchannel=none")), "RMS level dB:")
        };
        Measured {
            lufs: grab(&loudness, "I:"),
            true_peak: grab(&loudness, "Peak:"),
            low: rms("lowpass=f=120:poles=2,lowpass=f=120:poles=2,"),
            band: rms("highpass=f=200:poles=2,lowpass=f=1000:poles=2,"),
        }
    }
}
