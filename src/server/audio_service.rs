// both soundio and cpal use wasapi on windows and coreaudio on mac, they do not support loopback.
// libpulseaudio support loopback because pulseaudio is a standalone audio service with some
// configuration, but need to install the library and start the service on OS, not a good choice.
// windows: https://docs.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
// mac: https://github.com/mattingalls/Soundflower
// https://docs.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-iaudioclient
// https://github.com/ExistentialAudio/BlackHole

// if pactl not work, please run
// sudo apt-get --purge --reinstall install pulseaudio
// https://askubuntu.com/questions/403416/how-to-listen-live-sounds-from-input-from-external-sound-card
// https://wiki.debian.org/audio-loopback
// https://github.com/krruzic/pulsectl

use super::*;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
use hbb_common::anyhow::anyhow;
use magnum_opus::{Application::*, Channels::*, Encoder};
use std::sync::atomic::{AtomicBool, Ordering};

pub const NAME: &'static str = "audio";
pub const AUDIO_DATA_SIZE_U8: usize = 960 * 4; // 10ms in 48000 stereo
static RESTARTING: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref VOICE_CALL_INPUT_DEVICE: Arc::<Mutex::<Option<String>>> = Default::default();
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn new() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME.to_owned(), true);
    GenericService::repeat::<cpal_impl::State, _, _>(&svc.clone(), 33, cpal_impl::run);
    svc.sp
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn new() -> GenericService {
    let svc = EmptyExtraFieldService::new(NAME.to_owned(), true);
    GenericService::run(&svc.clone(), pa_impl::run);
    svc.sp
}

#[inline]
pub fn get_voice_call_input_device() -> Option<String> {
    VOICE_CALL_INPUT_DEVICE.lock().unwrap().clone()
}

#[inline]
pub fn set_voice_call_input_device(device: Option<String>, set_if_present: bool) {
    if !set_if_present && VOICE_CALL_INPUT_DEVICE.lock().unwrap().is_some() {
        return;
    }

    if *VOICE_CALL_INPUT_DEVICE.lock().unwrap() == device {
        return;
    }
    *VOICE_CALL_INPUT_DEVICE.lock().unwrap() = device;
    restart();
}

#[inline]
fn get_audio_input() -> String {
    VOICE_CALL_INPUT_DEVICE
        .lock()
        .unwrap()
        .clone()
        .unwrap_or(Config::get_option("audio-input"))
}

pub fn restart() {
    log::info!("restart the audio service, freezing now...");
    if RESTARTING.load(Ordering::SeqCst) {
        return;
    }
    RESTARTING.store(true, Ordering::SeqCst);
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod pa_impl {
    use super::*;

    // SAFETY: constrains of hbb_common::mem::aligned_u8_vec must be held
    unsafe fn align_to_32(data: Vec<u8>) -> Vec<u8> {
        if (data.as_ptr() as usize & 3) == 0 {
            return data;
        }

        let mut buf = vec![];
        buf = unsafe { hbb_common::mem::aligned_u8_vec(data.len(), 4) };
        buf.extend_from_slice(data.as_ref());
        buf
    }

    #[tokio::main(flavor = "current_thread")]
    pub async fn run(sp: EmptyExtraFieldService) -> ResultType<()> {
        hbb_common::sleep(0.1).await; // one moment to wait for _pa ipc
        RESTARTING.store(false, Ordering::SeqCst);
        #[cfg(target_os = "linux")]
        let mut stream = crate::ipc::connect(1000, "_pa").await?;
        unsafe {
            AUDIO_ZERO_COUNT = 0;
        }
        let mut encoder = Encoder::new(crate::platform::PA_SAMPLE_RATE, Stereo, LowDelay)?;
        #[cfg(target_os = "linux")]
        allow_err!(
            stream
                .send(&crate::ipc::Data::Config((
                    "audio-input".to_owned(),
                    Some(super::get_audio_input())
                )))
                .await
        );
        #[cfg(target_os = "linux")]
        let zero_audio_frame: Vec<f32> = vec![0.; AUDIO_DATA_SIZE_U8 / 4];
        #[cfg(target_os = "android")]
        let mut android_data = vec![];
        while sp.ok() && !RESTARTING.load(Ordering::SeqCst) {
            sp.snapshot(|sps| {
                sps.send(create_format_msg(crate::platform::PA_SAMPLE_RATE, 2));
                Ok(())
            })?;

            #[cfg(target_os = "linux")]
            if let Ok(data) = stream.next_raw().await {
                if data.len() == 0 {
                    send_f32(&zero_audio_frame, &mut encoder, &sp);
                    continue;
                }

                if data.len() != AUDIO_DATA_SIZE_U8 {
                    continue;
                }

                let data = unsafe { align_to_32(data.into()) };
                let data = unsafe {
                    std::slice::from_raw_parts::<f32>(data.as_ptr() as _, data.len() / 4)
                };
                send_f32(data, &mut encoder, &sp);
            }

            #[cfg(target_os = "android")]
            if scrap::android::ffi::get_audio_raw(&mut android_data, &mut vec![]).is_some() {
                let data = unsafe {
                    android_data = align_to_32(android_data);
                    std::slice::from_raw_parts::<f32>(
                        android_data.as_ptr() as _,
                        android_data.len() / 4,
                    )
                };
                send_f32(data, &mut encoder, &sp);
            } else {
                hbb_common::sleep(0.1).await;
            }
        }
        Ok(())
    }
}

#[inline]
#[cfg(feature = "screencapturekit")]
pub fn is_screen_capture_kit_available() -> bool {
    cpal::available_hosts()
        .iter()
        .any(|host| *host == cpal::HostId::ScreenCaptureKit)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod cpal_impl {
    use self::service::{Reset, ServiceSwap};
    use super::*;
    use cpal::{
        traits::{DeviceTrait, HostTrait, StreamTrait},
        BufferSize, Device, Host, InputCallbackInfo, StreamConfig, SupportedStreamConfig,
    };

    lazy_static::lazy_static! {
        static ref HOST: Host = cpal::default_host();
        static ref INPUT_BUFFER: Arc<Mutex<std::collections::VecDeque<f32>>> = Default::default();
        // KG3N mixed (system + mic) audio: per-source FIFOs + the source-clocked mixer
        // (crate::server::audio_mix). Callbacks push, the mixer thread pulls 10 ms frames.
        static ref MIX: Arc<Mutex<crate::server::audio_mix::Mixer>> = Default::default();
        static ref MIX_EPOCH: std::time::Instant = std::time::Instant::now();
    }

    /// Monotonic milliseconds for the mixer's "is this source still delivering" bookkeeping.
    fn now_ms() -> u64 {
        MIX_EPOCH.elapsed().as_millis() as u64
    }

    // mic watchdog restarts in a row (reset once the mic delivers again); capped at 3 so a mic that
    // is truly dead degrades to system-audio-only instead of restarting forever
    static MIC_WATCHDOG_RESTARTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // last device-error-triggered restart (throttle: at most one per 3 s)
    static LAST_ERR_RESTART_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[cfg(feature = "screencapturekit")]
    lazy_static::lazy_static! {
        static ref HOST_SCREEN_CAPTURE_KIT: Result<Host, cpal::HostUnavailable> = cpal::host_from_id(cpal::HostId::ScreenCaptureKit);
    }

    #[derive(Default)]
    pub struct State {
        stream: Option<AudioStream>,
        last_health_check: Option<std::time::Instant>,
    }

    struct AudioStream {
        _streams: Vec<Box<dyn StreamTrait>>,
        format: Arc<Message>,
        mixer_stop: Option<Arc<AtomicBool>>,
        mixer_thread: Option<std::thread::JoinHandle<()>>,
        // mixed streams only: device names at start (default-device change detection)
        sys_name: String,
        mic_name: String,
        started: std::time::Instant,
    }

    impl AudioStream {
        fn single(stream: Box<dyn StreamTrait>, format: Arc<Message>) -> Self {
            Self {
                _streams: vec![stream],
                format,
                mixer_stop: None,
                mixer_thread: None,
                sys_name: String::new(),
                mic_name: String::new(),
                started: std::time::Instant::now(),
            }
        }

        fn mixed(
            streams: Vec<Box<dyn StreamTrait>>,
            format: Arc<Message>,
            mixer_stop: Arc<AtomicBool>,
            mixer_thread: std::thread::JoinHandle<()>,
            sys_name: String,
            mic_name: String,
        ) -> Self {
            Self {
                _streams: streams,
                format,
                mixer_stop: Some(mixer_stop),
                mixer_thread: Some(mixer_thread),
                sys_name,
                mic_name,
                started: std::time::Instant::now(),
            }
        }
    }

    impl super::service::Reset for State {
        fn reset(&mut self) {
            if let Some(mut stream) = self.stream.take() {
                if let Some(stop) = stream.mixer_stop.take() {
                    stop.store(true, Ordering::SeqCst);
                }
                if let Some(thread) = stream.mixer_thread.take() {
                    let _ = thread.join();
                }
            }
        }
    }

    fn start_stream(sp: &GenericService) -> ResultType<AudioStream> {
        // Mix only for the normal "system audio" capture. During a voice call (or with a custom
        // audio-input) the capture source already IS a microphone, so mixing would open it twice
        // (+6 dB, comb filtering) and drop system audio.
        let mix_wanted = hbb_common::config::option2bool(
            "audio-mix-mic",
            &Config::get_option("audio-mix-mic"),
        );
        let voice_call = super::get_voice_call_input_device().is_some();
        let custom_input = !Config::get_option("audio-input").is_empty();
        if mix_wanted && !voice_call && !custom_input {
            match play_mixed(sp) {
                Ok(stream) => {
                    return Ok(stream);
                }
                Err(err) => {
                    log::warn!(
                        "Failed to start mixed audio, falling back to system audio: {}",
                        err
                    );
                }
            }
        }
        let (stream, format) = play(sp)?;
        Ok(AudioStream::single(stream, format))
    }

    fn run_restart(sp: EmptyExtraFieldService, state: &mut State) -> ResultType<()> {
        state.reset();
        sp.snapshot(|_sps: ServiceSwap<_>| Ok(()))?;
        match &state.stream {
            None => {
                state.stream = Some(start_stream(&sp)?);
            }
            _ => {}
        }
        if let Some(stream) = &state.stream {
            sp.send_shared(stream.format.clone());
        }
        RESTARTING.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn run_serv_snapshot(sp: EmptyExtraFieldService, state: &mut State) -> ResultType<()> {
        sp.snapshot(|sps| {
            match &state.stream {
                None => {
                    state.stream = Some(start_stream(&sp)?);
                }
                _ => {}
            }
            if let Some(stream) = &state.stream {
                sps.send_shared(stream.format.clone());
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn run(sp: EmptyExtraFieldService, state: &mut State) -> ResultType<()> {
        if !RESTARTING.load(Ordering::SeqCst) {
            check_mixed_health(state);
            if RESTARTING.load(Ordering::SeqCst) {
                return run_restart(sp, state);
            }
            run_serv_snapshot(sp, state)
        } else {
            run_restart(sp, state)
        }
    }

    /// Mixed stream self-healing, called from the service loop (~every 33 ms, cheap):
    /// - mixer thread gone -> drop the stream so the next snapshot recreates it
    /// - every 3 s: default output/input device changed -> restart (picks up a plugged headset)
    /// - every 3 s: mic delivered nothing for 2 s -> restart (max 3 in a row, then system-only)
    fn check_mixed_health(state: &mut State) {
        let dead_mixer = match &state.stream {
            Some(s) => s.mixer_thread.as_ref().map(|t| t.is_finished()).unwrap_or(false),
            None => false,
        };
        if dead_mixer {
            log::warn!("audio mixer thread had stopped; recreating the mixed audio stream");
            state.reset();
            return;
        }
        let (sys_name, mic_name, started) = match &state.stream {
            Some(s) if s.mixer_thread.is_some() => (s.sys_name.clone(), s.mic_name.clone(), s.started),
            _ => return,
        };
        let now = std::time::Instant::now();
        if let Some(t) = state.last_health_check {
            if now.duration_since(t) < std::time::Duration::from_secs(3) {
                return;
            }
        }
        state.last_health_check = Some(now);
        #[cfg(windows)]
        {
            let cur_sys = HOST
                .default_output_device()
                .and_then(|d| d.name().ok())
                .unwrap_or_default();
            let cur_mic = HOST
                .default_input_device()
                .and_then(|d| d.name().ok())
                .unwrap_or_default();
            if cur_sys != sys_name || cur_mic != mic_name {
                log::info!(
                    "default audio device changed ({} / {} -> {} / {}), restarting audio",
                    sys_name,
                    mic_name,
                    cur_sys,
                    cur_mic
                );
                super::restart();
                return;
            }
        }
        #[cfg(not(windows))]
        let _ = (&sys_name, &mic_name);
        if now.duration_since(started) < std::time::Duration::from_secs(3) {
            return;
        }
        let mic_age = MIX.lock().unwrap().mic.age_ms(now_ms());
        match mic_age {
            Some(a) if a <= 2000 => {
                MIC_WATCHDOG_RESTARTS.store(0, Ordering::SeqCst);
            }
            _ => {
                let n = MIC_WATCHDOG_RESTARTS.load(Ordering::SeqCst);
                if n < 3 {
                    MIC_WATCHDOG_RESTARTS.store(n + 1, Ordering::SeqCst);
                    log::warn!(
                        "microphone delivered no audio for 2s+, restarting audio ({}/3)",
                        n + 1
                    );
                    super::restart();
                }
            }
        }
    }

    fn send(
        data: Vec<f32>,
        sample_rate0: u32,
        sample_rate: u32,
        device_channel: u16,
        encode_channel: u16,
        encoder: &mut Encoder,
        sp: &GenericService,
    ) {
        let mut data = data;
        if sample_rate0 != sample_rate {
            data = crate::common::audio_resample(&data, sample_rate0, sample_rate, device_channel);
        }
        if device_channel != encode_channel {
            data = crate::common::audio_rechannel(
                data,
                sample_rate,
                sample_rate,
                device_channel,
                encode_channel,
            )
        }
        send_f32(&data, encoder, sp);
    }

    #[cfg(feature = "screencapturekit")]
    fn get_device() -> ResultType<(Device, SupportedStreamConfig)> {
        let audio_input = super::get_audio_input();
        if !audio_input.is_empty() {
            return get_audio_input(&audio_input);
        }
        if !is_screen_capture_kit_available() {
            return get_audio_input("");
        }
        let device = HOST_SCREEN_CAPTURE_KIT
            .as_ref()?
            .default_input_device()
            .with_context(|| "Failed to get default input device for loopback")?;
        let format = device
            .default_input_config()
            .map_err(|e| anyhow!(e))
            .with_context(|| "Failed to get input output format")?;
        log::info!("Default input format: {:?}", format);
        Ok((device, format))
    }

    #[cfg(windows)]
    fn get_device() -> ResultType<(Device, SupportedStreamConfig)> {
        let audio_input = super::get_audio_input();
        if !audio_input.is_empty() {
            return get_audio_input(&audio_input);
        }
        let device = HOST
            .default_output_device()
            .with_context(|| "Failed to get default output device for loopback")?;
        log::info!(
            "Default output device: {}",
            device.name().unwrap_or("".to_owned())
        );
        let format = device
            .default_output_config()
            .map_err(|e| anyhow!(e))
            .with_context(|| "Failed to get default output format")?;
        log::info!("Default output format: {:?}", format);
        Ok((device, format))
    }

    #[cfg(not(any(windows, feature = "screencapturekit")))]
    fn get_device() -> ResultType<(Device, SupportedStreamConfig)> {
        let audio_input = super::get_audio_input();
        get_audio_input(&audio_input)
    }

    fn get_audio_input(audio_input: &str) -> ResultType<(Device, SupportedStreamConfig)> {
        let mut device = None;
        #[cfg(feature = "screencapturekit")]
        if !audio_input.is_empty() && is_screen_capture_kit_available() {
            for d in HOST_SCREEN_CAPTURE_KIT
                .as_ref()?
                .devices()
                .with_context(|| "Failed to get audio devices")?
            {
                if d.name().unwrap_or("".to_owned()) == audio_input {
                    device = Some(d);
                    break;
                }
            }
        }
        if device.is_none() && !audio_input.is_empty() {
            for d in HOST
                .devices()
                .with_context(|| "Failed to get audio devices")?
            {
                if d.name().unwrap_or("".to_owned()) == audio_input {
                    device = Some(d);
                    break;
                }
            }
        }
        let device = device.unwrap_or(
            HOST.default_input_device()
                .with_context(|| "Failed to get default input device for loopback")?,
        );
        log::info!("Input device: {}", device.name().unwrap_or("".to_owned()));
        let format = device
            .default_input_config()
            .map_err(|e| anyhow!(e))
            .with_context(|| "Failed to get default input format")?;
        log::info!("Default input format: {:?}", format);
        Ok((device, format))
    }

    fn play(sp: &GenericService) -> ResultType<(Box<dyn StreamTrait>, Arc<Message>)> {
        use cpal::SampleFormat::*;
        let (device, config) = get_device()?;
        let sp = sp.clone();
        // Sample rate must be one of 8000, 12000, 16000, 24000, or 48000.
        let sample_rate_0 = config.sample_rate().0;
        let sample_rate = if sample_rate_0 < 12000 {
            8000
        } else if sample_rate_0 < 16000 {
            12000
        } else if sample_rate_0 < 24000 {
            16000
        } else if sample_rate_0 < 48000 {
            24000
        } else {
            48000
        };
        let ch = if config.channels() > 1 { Stereo } else { Mono };
        let stream = match config.sample_format() {
            I8 => build_input_stream::<i8>(device, &config, sp, sample_rate, ch)?,
            I16 => build_input_stream::<i16>(device, &config, sp, sample_rate, ch)?,
            I32 => build_input_stream::<i32>(device, &config, sp, sample_rate, ch)?,
            I64 => build_input_stream::<i64>(device, &config, sp, sample_rate, ch)?,
            U8 => build_input_stream::<u8>(device, &config, sp, sample_rate, ch)?,
            U16 => build_input_stream::<u16>(device, &config, sp, sample_rate, ch)?,
            U32 => build_input_stream::<u32>(device, &config, sp, sample_rate, ch)?,
            U64 => build_input_stream::<u64>(device, &config, sp, sample_rate, ch)?,
            F32 => build_input_stream::<f32>(device, &config, sp, sample_rate, ch)?,
            F64 => build_input_stream::<f64>(device, &config, sp, sample_rate, ch)?,
            f => bail!("unsupported audio format: {:?}", f),
        };
        stream.play()?;
        Ok((
            Box::new(stream),
            Arc::new(create_format_msg(sample_rate, ch as _)),
        ))
    }

    fn play_mixed(sp: &GenericService) -> ResultType<AudioStream> {
        use cpal::SampleFormat::*;
        const MIX_SAMPLE_RATE: u32 = 48_000;
        const MIX_CHANNELS: u16 = 2;
        let (sys_device, sys_config) = get_device()?;
        let sys_name = sys_device.name().unwrap_or("".to_owned());
        let mic_device = HOST
            .default_input_device()
            .with_context(|| "Failed to get default input device for microphone")?;
        let mic_name = mic_device.name().unwrap_or("".to_owned());
        log::info!("Default microphone input device: {}", mic_name);
        // A "Stereo Mix"-style default input, or the same device as the system source, would
        // double the system audio: use system audio only.
        if mic_name == sys_name || crate::server::audio_mix::is_loopback_input_name(&mic_name) {
            bail!(
                "default input '{}' is the system source or a loopback device; not mixing",
                mic_name
            );
        }
        let params = crate::server::audio_mix::MixParams::from_options(
            &Config::get_option("audio-mix-mic-gain-db"),
            &Config::get_option("audio-mix-sys-gain-db"),
            &Config::get_option("audio-mix-duck-db"),
            &Config::get_option("audio-mix-gate-db"),
        );
        log::info!("Mixed audio params: {:?}", params);
        let mic_config = mic_device
            .default_input_config()
            .map_err(|e| anyhow!(e))
            .with_context(|| "Failed to get default microphone input format")?;
        log::info!("Default microphone input format: {:?}", mic_config);
        MIX.lock().unwrap().clear();
        let sys_stream = match sys_config.sample_format() {
            I8 => build_mixed_input_stream::<i8>(sys_device, &sys_config, false)?,
            I16 => build_mixed_input_stream::<i16>(sys_device, &sys_config, false)?,
            I32 => build_mixed_input_stream::<i32>(sys_device, &sys_config, false)?,
            I64 => build_mixed_input_stream::<i64>(sys_device, &sys_config, false)?,
            U8 => build_mixed_input_stream::<u8>(sys_device, &sys_config, false)?,
            U16 => build_mixed_input_stream::<u16>(sys_device, &sys_config, false)?,
            U32 => build_mixed_input_stream::<u32>(sys_device, &sys_config, false)?,
            U64 => build_mixed_input_stream::<u64>(sys_device, &sys_config, false)?,
            F32 => build_mixed_input_stream::<f32>(sys_device, &sys_config, false)?,
            F64 => build_mixed_input_stream::<f64>(sys_device, &sys_config, false)?,
            f => bail!("unsupported audio format: {:?}", f),
        };
        let mic_stream = match mic_config.sample_format() {
            I8 => build_mixed_input_stream::<i8>(mic_device, &mic_config, true)?,
            I16 => build_mixed_input_stream::<i16>(mic_device, &mic_config, true)?,
            I32 => build_mixed_input_stream::<i32>(mic_device, &mic_config, true)?,
            I64 => build_mixed_input_stream::<i64>(mic_device, &mic_config, true)?,
            U8 => build_mixed_input_stream::<u8>(mic_device, &mic_config, true)?,
            U16 => build_mixed_input_stream::<u16>(mic_device, &mic_config, true)?,
            U32 => build_mixed_input_stream::<u32>(mic_device, &mic_config, true)?,
            U64 => build_mixed_input_stream::<u64>(mic_device, &mic_config, true)?,
            F32 => build_mixed_input_stream::<f32>(mic_device, &mic_config, true)?,
            F64 => build_mixed_input_stream::<f64>(mic_device, &mic_config, true)?,
            f => bail!("unsupported microphone audio format: {:?}", f),
        };
        sys_stream.play()?;
        mic_stream.play()?;
        unsafe {
            AUDIO_ZERO_COUNT = 0;
        }
        let mixer_stop = Arc::new(AtomicBool::new(false));
        let mixer_thread = start_mixer(sp.clone(), mixer_stop.clone(), params)?;
        Ok(AudioStream::mixed(
            vec![Box::new(sys_stream), Box::new(mic_stream)],
            Arc::new(create_format_msg(MIX_SAMPLE_RATE, MIX_CHANNELS)),
            mixer_stop,
            mixer_thread,
            sys_name,
            mic_name,
        ))
    }

    fn build_mixed_input_stream<T>(
        device: cpal::Device,
        config: &cpal::SupportedStreamConfig,
        is_mic: bool,
    ) -> ResultType<cpal::Stream>
    where
        T: cpal::SizedSample + dasp::sample::ToSample<f32> + Send + 'static,
    {
        let err_fn = move |err: cpal::StreamError| {
            if matches!(err, cpal::StreamError::DeviceNotAvailable) {
                // unplugged / invalidated device: restart so the new default device is opened
                log::warn!("mixed audio device no longer available ({}), restarting audio", err);
                let now = now_ms().max(1);
                let last = LAST_ERR_RESTART_MS.load(Ordering::SeqCst);
                if last == 0 || now.saturating_sub(last) > 3000 {
                    LAST_ERR_RESTART_MS.store(now, Ordering::SeqCst);
                    super::restart();
                }
            } else {
                log::trace!("an error occurred on mixed audio stream: {}", err);
            }
        };
        let sample_rate_0 = config.sample_rate().0;
        let device_channel = config.channels();
        // Channels first (at the device rate), then one stateful resampler per stream: correct for
        // >2-channel devices at any rate, and continuous across callback chunks.
        let mut resampler = crate::server::audio_mix::StreamResampler::new(
            sample_rate_0,
            crate::server::audio_mix::RATE,
        );
        let timeout = None;
        let stream_config = StreamConfig {
            channels: device_channel,
            sample_rate: config.sample_rate(),
            buffer_size: BufferSize::Default,
        };
        let stream = device.build_input_stream(
            &stream_config,
            move |data: &[T], _: &InputCallbackInfo| {
                let samples: Vec<f32> = data.iter().map(|s| T::to_sample(*s)).collect();
                let stereo = crate::server::audio_mix::to_stereo(&samples, device_channel as usize);
                let out = resampler.process(&stereo);
                let now = now_ms();
                let mut mix = MIX.lock().unwrap();
                if is_mic {
                    mix.mic.push(&out, now);
                } else {
                    mix.sys.push(&out, now);
                }
            },
            err_fn,
            timeout,
        )?;
        Ok(stream)
    }

    /// Mixer thread. Runs until `stop` (set by State::reset on restart / teardown); it no longer
    /// exits on its own when there are briefly no subscribers or while a restart is pending, which
    /// could leave a session silent. With no subscribers, frames are still drained (no stale
    /// backlog when someone reconnects) but not sent.
    fn start_mixer(
        sp: GenericService,
        stop: Arc<AtomicBool>,
        params: crate::server::audio_mix::MixParams,
    ) -> ResultType<std::thread::JoinHandle<()>> {
        let mut encoder = Encoder::new(48_000, Stereo, LowDelay)?;
        Ok(std::thread::spawn(move || {
            let mut dsp = crate::server::audio_mix::Dsp::new(params);
            let (mut sent, mut sys_pad, mut mic_pad) = (0u64, 0u64, 0u64);
            while !stop.load(Ordering::SeqCst) {
                let frame = MIX.lock().unwrap().next_frame(now_ms());
                match frame {
                    Some(f) => {
                        if f.sys_padded {
                            sys_pad += 1;
                        }
                        if f.mic_padded {
                            mic_pad += 1;
                        }
                        let out = dsp.process(&f.sys, &f.mic);
                        if sp.ok() {
                            send_f32(&out, &mut encoder, &sp);
                            sent += 1;
                        }
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(2)),
                }
            }
            let trimmed = MIX.lock().unwrap().trimmed;
            log::info!(
                "audio mixer stopped: {} frames sent, {} sys-padded, {} mic-padded, {} samples trimmed",
                sent,
                sys_pad,
                mic_pad,
                trimmed
            );
        }))
    }

    fn build_input_stream<T>(
        device: cpal::Device,
        config: &cpal::SupportedStreamConfig,
        sp: GenericService,
        sample_rate: u32,
        encode_channel: magnum_opus::Channels,
    ) -> ResultType<cpal::Stream>
    where
        T: cpal::SizedSample + dasp::sample::ToSample<f32>,
    {
        let err_fn = move |err| {
            // too many UnknownErrno, will improve later
            log::trace!("an error occurred on stream: {}", err);
        };
        let sample_rate_0 = config.sample_rate().0;
        log::debug!("Audio sample rate : {}", sample_rate);
        unsafe {
            AUDIO_ZERO_COUNT = 0;
        }
        let device_channel = config.channels();
        let mut encoder = Encoder::new(sample_rate, encode_channel, LowDelay)?;
        // https://www.opus-codec.org/docs/html_api/group__opusencoder.html#gace941e4ef26ed844879fde342ffbe546
        // https://chromium.googlesource.com/chromium/deps/opus/+/1.1.1/include/opus.h
        // Do not set `frame_size = sample_rate as usize / 100;`
        // Because we find `sample_rate as usize / 100` will cause encoder error in `encoder.encode_vec_float()` sometimes.
        // https://github.com/xiph/opus/blob/2554a89e02c7fc30a980b4f7e635ceae1ecba5d6/src/opus_encoder.c#L725
        let frame_size = sample_rate_0 as usize / 100; // 10 ms
        let encode_len = frame_size * encode_channel as usize;
        let rechannel_len = encode_len * device_channel as usize / encode_channel as usize;
        INPUT_BUFFER.lock().unwrap().clear();
        let timeout = None;
        let stream_config = StreamConfig {
            channels: device_channel,
            sample_rate: config.sample_rate(),
            buffer_size: BufferSize::Default,
        };
        let stream = device.build_input_stream(
            &stream_config,
            move |data: &[T], _: &InputCallbackInfo| {
                let buffer: Vec<f32> = data.iter().map(|s| T::to_sample(*s)).collect();
                let mut lock = INPUT_BUFFER.lock().unwrap();
                lock.extend(buffer);
                while lock.len() >= rechannel_len {
                    let frame: Vec<f32> = lock.drain(0..rechannel_len).collect();
                    send(
                        frame,
                        sample_rate_0,
                        sample_rate,
                        device_channel,
                        encode_channel as _,
                        &mut encoder,
                        &sp,
                    );
                }
            },
            err_fn,
            timeout,
        )?;
        Ok(stream)
    }
}

fn create_format_msg(sample_rate: u32, channels: u16) -> Message {
    let format = AudioFormat {
        sample_rate,
        channels: channels as _,
        ..Default::default()
    };
    let mut misc = Misc::new();
    misc.set_audio_format(format);
    let mut msg = Message::new();
    msg.set_misc(misc);
    msg
}

// use AUDIO_ZERO_COUNT for the Noise(Zero) Gate Attack Time
// every audio data length is set to 480
// MAX_AUDIO_ZERO_COUNT=800 is similar as Gate Attack Time 3~5s(Linux) || 6~8s(Windows)
const MAX_AUDIO_ZERO_COUNT: u16 = 800;
static mut AUDIO_ZERO_COUNT: u16 = 0;

fn send_f32(data: &[f32], encoder: &mut Encoder, sp: &GenericService) {
    if data.iter().filter(|x| **x != 0.).next().is_some() {
        unsafe {
            AUDIO_ZERO_COUNT = 0;
        }
    } else {
        unsafe {
            if AUDIO_ZERO_COUNT > MAX_AUDIO_ZERO_COUNT {
                if AUDIO_ZERO_COUNT == MAX_AUDIO_ZERO_COUNT + 1 {
                    log::debug!("Audio Zero Gate Attack");
                    AUDIO_ZERO_COUNT += 1;
                }
                return;
            }
            AUDIO_ZERO_COUNT += 1;
        }
    }
    #[cfg(target_os = "android")]
    {
        // the permitted opus data size are 120, 240, 480, 960, 1920, and 2880
        // if data size is bigger than BATCH_SIZE, AND is an integer multiple of BATCH_SIZE
        // then upload in batches
        const BATCH_SIZE: usize = 960;
        let input_size = data.len();
        if input_size > BATCH_SIZE && input_size % BATCH_SIZE == 0 {
            let n = input_size / BATCH_SIZE;
            for i in 0..n {
                match encoder
                    .encode_vec_float(&data[i * BATCH_SIZE..(i + 1) * BATCH_SIZE], BATCH_SIZE)
                {
                    Ok(data) => {
                        let mut msg_out = Message::new();
                        msg_out.set_audio_frame(AudioFrame {
                            data: data.into(),
                            ..Default::default()
                        });
                        sp.send(msg_out);
                    }
                    Err(_) => {}
                }
            }
        } else {
            log::debug!("invalid audio data size:{} ", input_size);
            return;
        }
    }

    #[cfg(not(target_os = "android"))]
    match encoder.encode_vec_float(data, data.len() * 6) {
        Ok(data) => {
            let mut msg_out = Message::new();
            msg_out.set_audio_frame(AudioFrame {
                data: data.into(),
                ..Default::default()
            });
            sp.send(msg_out);
        }
        Err(_) => {}
    }
}
